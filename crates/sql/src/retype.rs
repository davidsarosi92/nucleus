//! Changing a column's type online (PLAN.md §M14.18 O5): `ALTER TABLE t
//! ALTER COLUMN c TYPE ty [USING expr] CONCURRENTLY`.
//!
//! The conversion runs in transactions of its own, a part at a time, while
//! other sessions read and write the table; its state is the table's
//! record ([`catalog::Conversion`]), and each switch of phase is a small
//! transaction that rewrites it — every write reads the record validated,
//! so a write begun under the old phase fails (`40001`) and runs again
//! under the new one (as [`crate::convert`]'s storage changes do).
//!
//! - **Backfill.**  The converted value is kept as a hidden last value of
//!   each row: the row stores the table's columns and one more, which
//!   readers do not decode.  Writers ([`Shadow`]) compute it for every row
//!   they write — a value that does not convert, or breaks a check or
//!   `NOT NULL` under the new type, fails their statement — and keep the
//!   *shadow indexes*: the indexes on the column made again over the
//!   hidden value, with ids of their own (a unique one refuses a duplicate
//!   as the index would).  The rows without a hidden value are given
//!   theirs a part at a time, in primary key order.
//! - **Cutover** (one transaction): the column takes its new type, the
//!   shadow indexes take the old ones' places and names, the old ones'
//!   entries are left to remove.
//! - **Rewrite.**  A row still holding a hidden value is read with it as
//!   the column's — the row's length says which, no lookup — until it is
//!   rewritten, a part at a time; then the old indexes' entries are
//!   removed and the record cleared.
//! - **Abandon**, when the backfill finds a value that does not convert
//!   (or a duplicate, or a check broken): the shadows' entries and the
//!   hidden values are removed; the table is as it was; the statement
//!   fails with what was found.
//!
//! A conversion cut short (a crash, the session gone) leaves a table that
//! reads and writes correctly in whatever phase it was; running the
//! statement again, or the server's maintenance thread ([`resume_all`]),
//! finishes it.
//!
//! The conversion must give one value for one row whoever computes it —
//! each writer computes it in its own session — so it is immutable: no
//! function that may vary, no subquery or parameter, nothing that depends
//! on the session's time zone.

use std::collections::HashSet;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::catalog::{self, Conversion, ConversionPhase, Index, Table};
use crate::convert::attempt;
use crate::dml::{self, IndexKeys};
use crate::error::{Error, Result};
use crate::exec::pk_suffix;
use crate::expr::{Ctx, Env, Expr};
use crate::ext::Registry;
use crate::parser::parse_expr;
use crate::types::{DataType, Row, Value};

/// Rows given their hidden value, or rewritten, per transaction, at most:
/// the parts start at [`FIRST_PART`] and grow while they commit (each part
/// may meet the writes of others to the same rows, and run again
/// smaller).
const CHUNK_ROWS: usize = 4096;
/// The first part's rows.
const FIRST_PART: usize = 512;
/// Stale index entries removed per transaction.
const CLEAN_KEYS: usize = 16384;

/// How the values of column `c` of `t` convert to `to`: `using` (over the
/// row), else a taught assignment or implicit cast, else a built-in one.
pub(crate) fn conversion_expr(
    txn: &dyn KvTxn,
    reg: &Registry,
    t: &Table,
    c: usize,
    to: DataType,
    using: Option<&crate::ast::Expr>,
) -> Result<Expr> {
    let mut binder = crate::binder::Binder::new(txn, reg);
    if let Some(e) = using {
        return Ok(binder.expr(e, &dml::table_scope(t))?.0);
    }
    let from = t.columns[c].ty;
    let column_expr = Expr::Column(c);
    let name = |t: DataType| crate::builtin::type_of(t).name().to_string();
    let taught = catalog::get_cast(txn, &name(from), &name(to))?
        .filter(|k| k.context != catalog::CastContext::Explicit);
    if let Some(k) = taught {
        let f = reg.function(&k.function).cloned().ok_or_else(|| {
            Error::undefined_function(format!("function {} does not exist", k.function))
        })?;
        return Ok(Expr::Call(f, vec![column_expr]));
    }
    if from == to {
        Ok(column_expr)
    } else if to == DataType::Text
        || (from.is_number() && to.is_number())
        || (from.is_datetime() && to.is_datetime())
    {
        Ok(Expr::Cast(Box::new(column_expr), to))
    } else {
        Err(Error::type_mismatch(format!(
            "column \"{}\" cannot be cast automatically to type {}; give USING",
            t.columns[c].name,
            name(to)
        )))
    }
}

/// What writers of a table in [`ConversionPhase::Backfill`] do beside their
/// own work: bound once per statement.
pub struct Shadow {
    /// The table as written: its columns and the hidden one (the column
    /// under its new type), its indexes and the shadow indexes.
    pub view: Table,
    /// The hidden value's position.
    pub at: usize,
    conversion: Expr,
    /// The checks over the column, with its name read as the hidden value.
    checks: Vec<(String, Expr)>,
}

impl Shadow {
    /// The shadow of `t`, when its conversion is in the backfill.
    pub fn of(txn: &dyn KvTxn, reg: &Registry, t: &Table) -> Result<Option<Self>> {
        let Some(cv) = &t.converting else {
            return Ok(None);
        };
        if cv.phase != ConversionPhase::Backfill {
            return Ok(None);
        }
        let at = Conversion::hidden(t);
        let using = cv.using.as_deref().map(parse_expr).transpose()?;
        let conversion = conversion_expr(txn, reg, t, cv.column, cv.ty, using.as_ref())?;
        let mut view = t.clone();
        let mut col = t.columns[cv.column].clone();
        col.ty = cv.ty;
        col.max_len = cv.max_len;
        col.numeric = cv.numeric;
        col.default = None;
        col.generated = None;
        col.comment = None;
        view.columns.push(col);
        view.indexes.extend(cv.shadows.iter().cloned());
        // The checks reading the column, bound with its name naming the
        // hidden value.
        let mut swapped = view.clone();
        swapped.columns[cv.column].name = String::new();
        let scope = dml::table_scope(&swapped);
        let mut binder = crate::binder::Binder::new(txn, reg);
        let mut checks = Vec::new();
        for (name, text) in &t.checks {
            let e = binder.condition(&parse_expr(text)?, &scope)?;
            if e.columns().contains(&at) {
                checks.push((name.clone(), e));
            }
        }
        Ok(Some(Self {
            view,
            at,
            conversion,
            checks,
        }))
    }

    /// The converted value of `row` (the table's columns), fitted to the
    /// new type.
    fn convert(&self, row: &[Value], ctx: &Ctx) -> Result<Value> {
        let v = self.conversion.eval(&Env {
            row,
            outer: None,
            ctx,
        })?;
        dml::assign(ctx.txn, v, &self.view, self.at)
    }

    /// Gives a row being written its hidden value: an error when its value
    /// does not convert, or the converted row breaks `NOT NULL` or a check.
    pub fn fill(&self, row: &mut Row, ctx: &Ctx) -> Result<()> {
        row.truncate(self.at);
        let v = self.convert(row, ctx)?;
        let col = &self.view.columns[self.at];
        if v.is_null() && !col.nullable {
            return Err(Error::not_null_violation(format!(
                "column \"{}\" of relation \"{}\" would be NULL converted to its new type",
                col.name, self.view.name
            )));
        }
        row.push(v);
        let env = Env {
            row,
            outer: None,
            ctx,
        };
        for (name, check) in &self.checks {
            if check.eval(&env)?.truth()? == Some(false) {
                return Err(Error::check_violation(format!(
                    "the row of relation \"{}\" converted to the column's new type violates check constraint \"{name}\"",
                    self.view.name
                )));
            }
        }
        Ok(())
    }

    /// An old row (read: the table's columns, stored at `key`) with its
    /// hidden value as stored, if it has one — so its shadow entries are
    /// found to remove; a row stored without one has none.
    pub fn extend(&self, txn: &dyn KvTxn, key: &[u8], row: &mut Row) -> Result<()> {
        if row.len() == self.at
            && let Some(v) = hidden_at(txn, &self.view, key, self.at)?
        {
            row.push(v);
        }
        Ok(())
    }
}

/// Refuses what the online form does not convert (the transactional form
/// does — or refuses — the rest).
fn refuse(
    txn: &dyn KvTxn,
    reg: &Registry,
    t: &Table,
    c: usize,
    to: DataType,
    conversion: &Expr,
) -> Result<()> {
    let column = &t.columns[c].name;
    let no = |why: &str| {
        Err(Error::feature(format!(
            "ALTER COLUMN … TYPE … CONCURRENTLY cannot change column \"{column}\": {why}; without CONCURRENTLY it runs in one transaction"
        )))
    };
    if t.partition_by.is_some() || t.partition_of.is_some() {
        return no("the table is partitioned or a partition");
    }
    if t.storage != crate::storage::ROW {
        return no("the table is not stored as rows alone");
    }
    if t.chunks.is_some() || t.memory.is_some() {
        return no("the table is a chunk table or a memory store");
    }
    if t.primary_key.contains(&c) {
        return no("it is in the primary key");
    }
    if t.columns[c].generated.is_some() {
        return no("it is generated");
    }
    if t.expiry == Some(c) {
        return no("it is the table's expiry column");
    }
    if t.converting.is_some() {
        return no("another column's type is being changed");
    }
    if !t.gone.is_empty() {
        // A row's hidden value goes after its stored places; the
        // transactional form rewrites the rows whole, without them.
        return no("its rows still store a dropped column's place");
    }
    if !t.building.is_empty() {
        return no("an index of the table is being built");
    }
    if !t.validating.is_empty() {
        return no("a column's type is being checked");
    }
    for ix in t.indexes.iter().filter(|ix| ix.uses(c)) {
        let own = ix.columns.iter().zip(&ix.exprs).zip(&ix.orderings);
        if ix.method.is_some()
            || own
                .clone()
                .any(|((&k, e), o)| e.is_some() || (k == c && o.is_some()))
        {
            return no(&format!(
                "index \"{}\" is an expression's, a method's or in a taught ordering",
                ix.name
            ));
        }
    }
    let referenced = t.referenced_by.iter().any(|(child, name)| {
        catalog::table(txn, child).is_ok_and(|ct| {
            ct.foreign_keys
                .iter()
                .any(|fk| &fk.name == name && fk.ref_columns.contains(&c))
                || ct
                    .chunks
                    .as_ref()
                    .is_some_and(|s| s.fk == *name && s.column == c)
        })
    });
    if referenced || t.foreign_keys.iter().any(|fk| fk.columns.contains(&c)) {
        return no("a foreign key or a chunk table uses it");
    }
    if crate::chunk_tables::derives(t, c) {
        return no("a chunk table derives it");
    }
    for col in &t.columns {
        if let Some(g) = &col.generated
            && generated_reads(txn, reg, t, g, c)?
        {
            return no("a generated column reads it");
        }
    }
    let from = t.columns[c].ty;
    if from != to && (from == DataType::TimestampTz || to == DataType::TimestampTz) {
        return no("a conversion from or to timestamptz depends on the session's time zone");
    }
    if let Some(p) = dml::immutability_problem(conversion, t) {
        return no(&format!("its conversion uses {p}"));
    }
    Ok(())
}

/// Whether generated column definition `g` reads column `c` of `t`.
fn generated_reads(
    txn: &dyn KvTxn,
    reg: &Registry,
    t: &Table,
    g: &crate::ast::Generated,
    c: usize,
) -> Result<bool> {
    let text = match g {
        crate::ast::Generated::Stored(e) => e,
        crate::ast::Generated::Embedding { source, .. } => source,
    };
    let mut binder = crate::binder::Binder::new(txn, reg);
    let e = binder.expr(&parse_expr(text)?, &dml::table_scope(t))?.0;
    Ok(e.columns().contains(&c))
}

/// The conversions running in this process, by store and table: the
/// maintenance thread leaves them to their statements.
fn running() -> &'static Mutex<HashSet<(usize, String)>> {
    static R: std::sync::OnceLock<Mutex<HashSet<(usize, String)>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Marks a conversion (or another change run in parts, by its key) as run
/// here until dropped; `None` if it already is.
pub(crate) struct Running((usize, String));

impl Running {
    pub(crate) fn claim(kv: &Arc<dyn Kv>, table: &str) -> Option<Self> {
        let key = (Arc::as_ptr(kv) as *const () as usize, table.to_string());
        running()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone())
            .then_some(Self(key))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        running()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// `ALTER TABLE table ALTER COLUMN column TYPE ty [USING using]
/// CONCURRENTLY`: returns when the column has its new type and the rows
/// are rewritten (see the module).
#[allow(clippy::too_many_arguments)]
pub fn alter(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    table: &str,
    column: &str,
    ty: &crate::ast::SqlType,
    typmod: &crate::ast::TypeMod,
    using: Option<(&crate::ast::Expr, &str)>,
) -> Result<()> {
    let Some(_running) = Running::claim(kv, table) else {
        return Err(Error::object_in_use(format!(
            "a column of \"{table}\" is having its type changed by another session"
        )));
    };
    // `false`: the same conversion, cut short — finished all the same.
    attempt(kv, |txn| start(txn, reg, table, column, ty, typmod, using))?;
    finish(kv, reg, table)
}

/// `ALTER COLUMN … TYPE` (`stmt`, without `CONCURRENTLY`) whose rewrite
/// in one transaction met a write committed under it: converted online
/// instead, when it is a rewrite this can do (`Ok(false)` otherwise: the
/// statement runs again as it was).
pub fn fall_back(kv: &Arc<dyn Kv>, reg: &Registry, stmt: &crate::ast::Statement) -> Result<bool> {
    let crate::ast::Statement::AlterColumnType {
        table,
        column,
        ty,
        typmod,
        using,
        using_text,
        concurrently: false,
    } = stmt
    else {
        return Ok(false);
    };
    {
        let txn = kv.begin(TxnOptions::read_only())?;
        let t = catalog::table(&*txn, table)?;
        let Some(c) = t.column(column) else {
            return Ok(false);
        };
        let to = crate::binder::column_type(&*txn, reg, ty)?;
        // Values kept as they are: the catalog's change (or a check).
        let fit = crate::validating::fit(&t.columns[c], to, typmod.max_len, typmod.numeric);
        if using.is_none()
            && fit != crate::validating::Fit::Rewrite
            && crate::validating::in_place(&t, c)
        {
            return Ok(false);
        }
        let conversion = conversion_expr(&*txn, reg, &t, c, to, using.as_ref())?;
        match refuse(&*txn, reg, &t, c, to, &conversion) {
            Ok(()) => {}
            Err(e) if e.code == "0A000" => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    let using = using.as_ref().zip(using_text.as_deref());
    alter(kv, reg, table, column, ty, typmod, using)?;
    Ok(true)
}

/// Records the conversion (the backfill begins); `false` when the same one
/// is already recorded (cut short: to finish).
fn start(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    ty: &crate::ast::SqlType,
    typmod: &crate::ast::TypeMod,
    using: Option<(&crate::ast::Expr, &str)>,
) -> Result<bool> {
    let mut t = catalog::table(&*txn, table)?;
    let c = t.column(column).ok_or_else(|| {
        Error::undefined_column(format!(
            "column \"{column}\" of relation \"{table}\" does not exist"
        ))
    })?;
    let to = crate::binder::column_type(&*txn, reg, ty)?;
    crate::binder::check_modifier(to, typmod.max_len)?;
    if let Some(cv) = &t.converting
        && cv.column == c
        && cv.ty == to
        && cv.max_len == typmod.max_len
        && cv.numeric == typmod.numeric
        && cv.using.as_deref() == using.map(|(_, s)| s)
    {
        return Ok(false);
    }
    let conversion = conversion_expr(&*txn, reg, &t, c, to, using.map(|(e, _)| e))?;
    refuse(&*txn, reg, &t, c, to, &conversion)?;
    // The table as it will be: its checks, default and keys must hold
    // under the new type.
    let mut new = t.clone();
    new.columns[c].ty = to;
    new.columns[c].max_len = typmod.max_len;
    new.columns[c].numeric = typmod.numeric;
    for ix in new.indexes.iter().filter(|ix| ix.uses(c)) {
        crate::session::require_keys(&new.columns, &[c], ix.unique, "an index")?;
    }
    {
        let w = dml::Writer::new(&*txn, reg, new.clone())?;
        if new.columns[c].default.is_some() {
            w.default_of(c, &Ctx::new(&*txn, reg, &[]))?;
        }
    }
    let at = Conversion::hidden(&t);
    let mut shadows = Vec::new();
    for ix in t.indexes.iter().filter(|ix| ix.uses(c)) {
        let mut s = Index::plain(
            t.next_index_id,
            ix.name.clone(),
            ix.columns
                .iter()
                .map(|&k| if k == c { at } else { k })
                .collect(),
            ix.unique,
        );
        s.orderings = ix.orderings.clone();
        t.next_index_id += 1;
        shadows.push(s);
    }
    t.converting = Some(Conversion {
        column: c,
        ty: to,
        max_len: typmod.max_len,
        numeric: typmod.numeric,
        using: using.map(|(_, s)| s.to_string()),
        shadows,
        phase: ConversionPhase::Backfill,
        stale: Vec::new(),
    });
    catalog::update_table(txn, &t)?;
    Ok(true)
}

/// Whether an error found by the backfill is the data's — the conversion
/// cannot be done, and is abandoned — rather than the system's (left to
/// run again).
fn data_error(e: &Error) -> bool {
    e.code.starts_with("22") || e.code.starts_with("23") || e.code == "42804"
}

/// Finishes `table`'s conversion from whatever phase it is in.
pub fn finish(kv: &Arc<dyn Kv>, reg: &Registry, table: &str) -> Result<()> {
    loop {
        let t = attempt(kv, |txn| catalog::table(&*txn, table))?;
        let Some(cv) = &t.converting else {
            return Ok(());
        };
        match cv.phase {
            ConversionPhase::Backfill => match backfill(kv, reg, table) {
                Ok(()) => cutover(kv, reg, table)?,
                Err(e) if data_error(&e) => {
                    abandon(kv, table)?;
                    finish(kv, reg, table)?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            },
            ConversionPhase::Rewrite | ConversionPhase::Abandon => {
                rewrite(kv, table)?;
                drop_stale(kv, table)?;
                attempt(kv, |txn| {
                    let mut t = catalog::table(&*txn, table)?;
                    t.converting = None;
                    catalog::update_table(txn, &t)
                })?;
            }
        }
    }
}

/// Runs `part` — given where the last part ended and how many rows to
/// take — a transaction each, until it returns `None`.  A part that
/// conflicts with the table's writers runs again smaller (down to a row),
/// one that commits lets the next grow back: under many writes the parts
/// shrink until they slip between them.
pub(crate) fn in_parts(
    kv: &Arc<dyn Kv>,
    part: impl FnMut(&mut dyn KvTxn, Option<&[u8]>, usize) -> Result<Option<Vec<u8>>>,
) -> Result<()> {
    in_parts_watched(kv, part, &mut |_| ())
}

/// [`in_parts`], `after` told the last key of each part committed.
pub(crate) fn in_parts_watched(
    kv: &Arc<dyn Kv>,
    mut part: impl FnMut(&mut dyn KvTxn, Option<&[u8]>, usize) -> Result<Option<Vec<u8>>>,
    after_part: &mut dyn FnMut(&[u8]),
) -> Result<()> {
    let mut after: Option<Vec<u8>> = None;
    let mut size = FIRST_PART;
    let mut tries = 0;
    loop {
        let mut txn = kv.begin(nucleus_kv::TxnOptions::default())?;
        let r = part(txn.as_mut(), after.as_deref(), size)
            .and_then(|v| txn.commit().map(|_| v).map_err(Error::from));
        match r {
            Ok(Some(k)) => {
                after_part(&k);
                after = Some(k);
                tries = 0;
                size = (size * 2).min(CHUNK_ROWS);
            }
            Ok(None) => return Ok(()),
            Err(e) if e.code == "40001" && tries < 1000 => {
                tries += 1;
                size = (size / 2).max(1);
                kv.back_off(tries.min(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Up to `size` stored rows of `t` after `after`: their keys and stored
/// values.
fn rows_after(
    txn: &dyn KvTxn,
    t: &Table,
    after: Option<&[u8]>,
    size: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let start = t.index_prefix(0);
    let end = t.index_end(0);
    let from = match after {
        Some(k) => Bound::Excluded(k),
        None => Bound::Included(start.as_slice()),
    };
    txn.scan(from, Bound::Excluded(&end[..]))?
        .take(size)
        .map(|e| e.map_err(Into::into))
        .collect()
}

/// The values a stored row holds.
fn stored_len(bytes: &[u8]) -> Result<usize> {
    Ok(crate::encoding::row_header(bytes)?.n)
}

/// Gives every row without a hidden value its own, with its shadow index
/// entries, a part a transaction; stops early when the phase changed.
fn backfill(kv: &Arc<dyn Kv>, reg: &Registry, table: &str) -> Result<()> {
    in_parts(kv, |txn, after, size| {
        let t = catalog::table(&*txn, table)?;
        catalog::validate_table(&*txn, table)?;
        let Some(sh) = Shadow::of(&*txn, reg, &t)? else {
            return Ok(None);
        };
        let keys = IndexKeys::new(&*txn, reg, &sh.view)?;
        let shadows: Vec<usize> = (t.indexes.len()..sh.view.indexes.len()).collect();
        let stored = rows_after(&*txn, &t, after, size)?;
        let Some((last, _)) = stored.last() else {
            return Ok(None);
        };
        let last = last.clone();
        // Converted first (reading), then written.
        let mut writes = Vec::new();
        {
            let ctx = Ctx::new(&*txn, reg, &[]);
            for (key, bytes) in &stored {
                if stored_len(bytes)? > sh.at {
                    continue; // written since the backfill began
                }
                let row = crate::large::read_row(&*txn, &t, key, bytes)?;
                let mut ext = row.clone();
                sh.fill(&mut ext, &ctx)?;
                let pk = pk_suffix(&t, key);
                let mut entries = Vec::with_capacity(shadows.len());
                for &i in &shadows {
                    entries.push((i, keys.entry(&*txn, &sh.view, &ext, pk, i)?));
                }
                writes.push((key.clone(), row, ext, entries));
            }
        }
        let storage = crate::storage::of(&t)?;
        for (key, row, ext, entries) in writes {
            storage.write(txn, &sh.view, &key, &ext, Some(&row))?;
            let pk = pk_suffix(&t, &key).to_vec();
            for (i, (k, unique)) in entries
                .into_iter()
                .flat_map(|(i, es)| es.into_iter().map(move |e| (i, e)))
            {
                if unique && txn.get(&k)?.is_some() {
                    return Err(Error::unique_violation(format!(
                        "converted to the column's new type, two rows have the same value in unique index \"{}\"",
                        sh.view.indexes[i].name
                    )));
                }
                txn.put(&k, &pk)?;
            }
        }
        Ok(Some(last))
    })
}

/// The column takes its new type; the shadow indexes take the old ones'
/// places.
fn cutover(kv: &Arc<dyn Kv>, reg: &Registry, table: &str) -> Result<()> {
    attempt(kv, |txn| {
        let mut t = catalog::table(&*txn, table)?;
        let Some(cv) = t.converting.clone() else {
            return Ok(());
        };
        if cv.phase != ConversionPhase::Backfill {
            return Ok(());
        }
        let (c, at) = (cv.column, Conversion::hidden(&t));
        t.columns[c].ty = cv.ty;
        t.columns[c].max_len = cv.max_len;
        t.columns[c].numeric = cv.numeric;
        let mut stale = Vec::new();
        for ix in &mut t.indexes {
            if !ix.uses(c) {
                continue;
            }
            let shadow = cv
                .shadows
                .iter()
                .find(|s| s.name == ix.name)
                .ok_or_else(|| Error::internal(format!("index {} has no shadow", ix.name)))?;
            stale.push(ix.id);
            let mut n = shadow.clone();
            for k in &mut n.columns {
                if *k == at {
                    *k = c;
                }
            }
            *ix = n;
        }
        t.converting = Some(Conversion {
            shadows: Vec::new(),
            phase: ConversionPhase::Rewrite,
            stale,
            ..cv
        });
        // Its checks and default bound against the new type.
        dml::Writer::new(&*txn, reg, t.clone())?;
        txn.delete(&catalog::stats_key(&t))?; // the old type's statistics
        catalog::delete_stats_sample(txn, &t)?;
        catalog::note_stored_types(txn, &t.columns)?;
        catalog::update_table(txn, &t)
    })
}

/// The backfill failed: the shadows' entries and the hidden values to
/// remove.
fn abandon(kv: &Arc<dyn Kv>, table: &str) -> Result<()> {
    attempt(kv, |txn| {
        let mut t = catalog::table(&*txn, table)?;
        let Some(cv) = t.converting.as_mut() else {
            return Ok(());
        };
        if cv.phase != ConversionPhase::Backfill {
            return Ok(());
        }
        cv.stale.extend(cv.shadows.iter().map(|s| s.id));
        cv.shadows.clear();
        cv.phase = ConversionPhase::Abandon;
        catalog::update_table(txn, &t)
    })
}

/// Rewrites every row still holding a hidden value, a part a transaction:
/// after the cutover with it as the column's value, after a failure
/// without it.
fn rewrite(kv: &Arc<dyn Kv>, table: &str) -> Result<()> {
    in_parts(kv, |txn, after, size| {
        let t = catalog::table(&*txn, table)?;
        catalog::validate_table(&*txn, table)?;
        let n = t.columns.len();
        let stored = rows_after(&*txn, &t, after, size)?;
        let Some((last, _)) = stored.last() else {
            return Ok(None);
        };
        let last = last.clone();
        let mut rows = Vec::new();
        for (key, bytes) in &stored {
            if stored_len(bytes)? > n {
                rows.push((key.clone(), crate::large::read_row(&*txn, &t, key, bytes)?));
            }
        }
        let storage = crate::storage::of(&t)?;
        for (key, row) in rows {
            storage.write(txn, &t, &key, &row, Some(&row))?;
        }
        Ok(Some(last))
    })
}

/// Removes the stale indexes' entries, a part a transaction.
fn drop_stale(kv: &Arc<dyn Kv>, table: &str) -> Result<()> {
    let t = attempt(kv, |txn| catalog::table(&*txn, table))?;
    let Some(cv) = &t.converting else {
        return Ok(());
    };
    for &id in &cv.stale {
        remove_entries(kv, &t, id)?;
    }
    Ok(())
}

/// Removes the entries of index `id` of `t` (no longer in its record), a
/// part a transaction.
pub(crate) fn remove_entries(kv: &Arc<dyn Kv>, t: &Table, id: u32) -> Result<()> {
    let (p, end) = (t.index_prefix(id), t.index_end(id));
    // From where the last part ended: the keys before it are deleted
    // versions a scan from the start would walk again.
    let mut after: Option<Vec<u8>> = None;
    loop {
        let last = attempt(kv, |txn| {
            let from = after
                .as_deref()
                .map_or(Bound::Included(p.as_slice()), Bound::Excluded);
            let keys: Vec<Vec<u8>> = txn
                .scan(from, Bound::Excluded(&end[..]))?
                .take(CLEAN_KEYS)
                .map(|e| e.map(|(k, _)| k))
                .collect::<nucleus_kv::Result<_>>()?;
            for k in &keys {
                txn.delete(k)?;
            }
            Ok(keys.last().cloned())
        })?;
        match last {
            Some(k) => after = Some(k),
            None => return Ok(()),
        }
    }
}

/// The tables whose column's type is being changed, read in a read-only
/// transaction: the maintenance thread's look, run beside every session,
/// conflicts with none of their changes of the catalog.
fn converting(kv: &Arc<dyn Kv>) -> Result<Vec<String>> {
    let txn = kv.begin(TxnOptions::read_only())?;
    Ok(catalog::tables(&*txn)?
        .into_iter()
        .filter(|t| t.converting.is_some())
        .map(|t| t.name)
        .collect())
}

/// Finishes every conversion cut short (the maintenance thread): those
/// not run by a statement in this process.  Returns the tables finished;
/// a conversion that failed is reported and left (its statement's error
/// is the table's again at the next try).
pub fn resume_all(kv: &Arc<dyn Kv>, reg: &Registry) -> Result<Vec<String>> {
    let names = converting(kv)?;
    let mut done = Vec::new();
    for name in names {
        let Some(_running) = Running::claim(kv, &name) else {
            continue;
        };
        match finish(kv, reg, &name) {
            Ok(()) => done.push(name),
            Err(e) if data_error(&e) => done.push(name), // abandoned: the table as before
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Settles every conversion of a restored database (whose secondary
/// indexes are rebuilt from its rows): one before its cutover is abandoned
/// — its shadow indexes were not backed up — and every one finished.
pub(crate) fn settle(kv: &Arc<dyn Kv>, reg: &Registry) -> Result<()> {
    let names = converting(kv)?;
    for name in names {
        abandon(kv, &name)?;
        finish(kv, reg, &name)?;
    }
    Ok(())
}

/// Whether `t` may take a change of definition other than its
/// conversion's own: refused while a column's type is being changed.
pub(crate) fn allows_change(old: &Table, new: &Table) -> Result<()> {
    let Some(cv) = &old.converting else {
        return Ok(());
    };
    if new.converting != old.converting {
        return Ok(()); // the conversion's own switch
    }
    let same = |a: &Table, b: &Table| {
        a.columns == b.columns
            && a.primary_key == b.primary_key
            && a.indexes == b.indexes
            && a.checks == b.checks
            && a.foreign_keys == b.foreign_keys
            && a.referenced_by == b.referenced_by
            && a.storage == b.storage
            && a.copy == b.copy
            && a.partition_by == b.partition_by
            && a.partition_of == b.partition_of
            && a.chunks == b.chunks
            && a.expiry == b.expiry
            && a.subject == b.subject
            && a.next_index_id == b.next_index_id
    };
    if same(old, new) {
        return Ok(());
    }
    Err(Error::object_in_use(format!(
        "the type of column \"{}\" of \"{}\" is being changed; its definition can change when that is done",
        old.columns[cv.column].name, old.name
    )))
}

/// The hidden value of the row stored at `key` of `t`, if it has one.
pub(crate) fn stored_hidden(txn: &dyn KvTxn, t: &Table, key: &[u8]) -> Result<Option<Value>> {
    hidden_at(txn, t, key, Conversion::hidden(t))
}

/// The value at position `at` of the row stored at `key` of `t` (`t`'s
/// id and key: a view of it will do), if it has one.
fn hidden_at(txn: &dyn KvTxn, t: &Table, key: &[u8], at: usize) -> Result<Option<Value>> {
    let Some(bytes) = txn.get(key)? else {
        return Ok(None);
    };
    if stored_len(&bytes)? <= at {
        return Ok(None);
    }
    let mut wanted = vec![false; at + 1];
    wanted[at] = true;
    let row = crate::encoding::decode_some(&bytes, at + 1, Some(&wanted), &mut |c, len| {
        crate::large::fetch(txn, t, pk_suffix(t, key), c, len)
    })?;
    Ok(row.into_iter().nth(at))
}

/// Reads a stored row of a table being converted, as its readers see it:
/// the table's columns — a hidden value left out, or (after the cutover)
/// taken as the column's.  `wanted` as [`crate::encoding::decode_some`]'s.
pub(crate) fn read_some(
    t: &Table,
    cv: &Conversion,
    bytes: &[u8],
    wanted: Option<&[bool]>,
    resolve: &mut dyn FnMut(usize, u32) -> Result<Value>,
) -> Result<Row> {
    let n = t.columns.len();
    let stored = stored_len(bytes)?;
    if stored <= n {
        return crate::encoding::decode_some(bytes, n, wanted, resolve);
    }
    let overlay = cv.phase == ConversionPhase::Rewrite;
    let c = cv.column;
    // The hidden value read in the column's place, the column's own not.
    let mut mask: Vec<bool> = match wanted {
        Some(w) => (0..n).map(|i| w.get(i).copied().unwrap_or(false)).collect(),
        None => vec![true; n],
    };
    let want_c = mask[c];
    mask.push(overlay && want_c);
    if overlay {
        mask[c] = false;
    }
    let mut row = crate::encoding::decode_some(bytes, n, Some(&mask), resolve)?;
    let hidden = row.pop().unwrap_or(Value::Null);
    row.truncate(n);
    if overlay && want_c {
        row[c] = hidden;
    }
    Ok(row)
}
