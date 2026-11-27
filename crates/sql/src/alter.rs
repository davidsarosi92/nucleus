//! The `ALTER` a migration needs (PLAN.md §M14.16 M3): `ALTER TABLE …
//! RENAME TO`, `RENAME COLUMN`, `RENAME CONSTRAINT`, `DROP COLUMN`,
//! `ALTER COLUMN … SET / DROP DEFAULT`, `SET / DROP NOT NULL`, and
//! `ALTER INDEX … RENAME TO` — transactional as the rest of DDL.
//!
//! **What names a table or a column keeps up.**  The catalog holds
//! columns by position (indexes, keys, foreign keys, statistics, the
//! column copy, the subject and expiry columns, a chunk table's text
//! column) and tables by name (their definition's key, foreign keys
//! both ways, partitions, policies, ACLs, tokens scoped to tables); the
//! expressions it keeps — checks, generated columns, index expressions,
//! policies — are SQL text.  A rename rewrites what holds the name: the
//! records keyed by it, and the identifiers in those texts (SQLite's way
//! for `RENAME COLUMN`; PostgreSQL keeps parse trees by number instead,
//! which nucleus's texts are not).  Each text rewritten is bound again
//! and must read the same columns it read before — a rewrite that would
//! change what an expression means is refused, not stored.
//!
//! **A dropped column** is removed from every row (rows are positional:
//! the rest move one place down), as `ALTER COLUMN … TYPE` rewrites
//! them; what is wholly about it goes with it — its indexes, checks,
//! foreign keys and its place in statistics (PostgreSQL's rule) — and
//! what else names it is refused unless `CASCADE` drops it too (other
//! tables' foreign keys, policies), or refused outright where dropping
//! would change more than the column (the primary key, the partition key,
//! a generated column's source, a chunk table's text, the subject and
//! expiry columns).  Not taken: PostgreSQL's dropped-column mark
//! (`attisdropped`), which makes the drop O(1) but leaves every reader of
//! a row skipping a hole; noted for when tables are big enough to need it.

use nucleus_kv::KvTxn;

use crate::ast::TableChange;
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::expr::Ctx;
use crate::ext::Registry;
use crate::lexer::{Token, tokenize_spans};
use crate::policies::{self, Policy};
use crate::privileges::{self, Kind};

/// `ALTER TABLE table change`.
pub(crate) fn alter_table(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    change: &TableChange,
) -> Result<()> {
    // A property graph's definition names its tables and columns.
    match change {
        TableChange::Rename(_) => crate::graphs::check_unused(&*txn, table, None)?,
        TableChange::RenameColumn { from, .. } => {
            crate::graphs::check_unused(&*txn, table, Some(from))?
        }
        TableChange::DropColumn {
            column, cascade, ..
        } => crate::graphs::cascade(txn, table, Some(column), *cascade)?,
        _ => {}
    }
    match change {
        TableChange::Rename(to) => {
            rename_table(txn, reg, table, to)?;
            // The sequences its columns own follow.
            crate::sequences::follow_rename(txn, table, None, to)
        }
        TableChange::RenameConstraint { from, to } => rename_constraint(txn, table, from, to),
        TableChange::RenameColumn { from, to } => {
            for t in family(&*txn, table, "rename a column of")? {
                rename_column(txn, reg, &t, from, to)?;
            }
            crate::sequences::follow_rename(txn, table, Some(from), to)
        }
        TableChange::DropColumn {
            column,
            if_exists,
            cascade,
        } => {
            let t = catalog::table(&*txn, table)?;
            if t.column(column).is_none() && *if_exists {
                return Ok(());
            }
            for t in family(&*txn, table, "drop a column of")? {
                drop_column(txn, reg, &t, column, *cascade)?;
            }
            crate::sequences::drop_owned(txn, table, Some(column), *cascade)
        }
        TableChange::SetDefault { column, default } => {
            for t in family(&*txn, table, "change a default of")? {
                set_default(txn, reg, &t, column, default.as_deref())?;
            }
            Ok(())
        }
        TableChange::Identity { column, change } => identity(txn, table, column, change),
        TableChange::SetNotNull { column, not_null } => {
            for t in family(&*txn, table, "change a column of")? {
                set_not_null(txn, reg, &t, column, *not_null)?;
            }
            Ok(())
        }
    }
}

/// The tables a column change applies to: a partitioned table's
/// partitions, then it; a partition's columns are its parent's to
/// change.
fn family(txn: &dyn KvTxn, table: &str, what: &str) -> Result<Vec<String>> {
    let t = catalog::table(txn, table)?;
    if let Some((parent, _)) = &t.partition_of {
        return Err(Error::wrong_object_type(format!(
            "cannot {what} partition \"{table}\": change its partitioned table \"{parent}\""
        )));
    }
    let mut out = Vec::new();
    if t.partition_by.is_some() {
        out.extend(
            crate::partition::partitions(txn, &t)?
                .into_iter()
                .map(|p| p.name),
        );
    }
    out.push(t.name);
    Ok(out)
}

fn column_of(t: &Table, name: &str) -> Result<usize> {
    t.column(name).ok_or_else(|| {
        Error::undefined_column(format!(
            "column \"{name}\" of relation \"{}\" does not exist",
            t.name
        ))
    })
}

/// Refuses changing column `c` of a memory store or a chunk table that
/// the engine itself keeps.
fn refuse_kept(t: &Table, c: usize, what: &str) -> Result<()> {
    let kept_by = if t.memory.is_some() && c < crate::definitions::MEMORY_COLUMNS {
        "memory store"
    } else if crate::chunk_tables::derives(t, c) {
        "chunk table"
    } else {
        return Ok(());
    };
    Err(Error::wrong_object_type(format!(
        "cannot {what} column \"{}\": {kept_by} \"{}\" keeps it",
        t.columns[c].name, t.name
    )))
}

// ------------------------------------------------------- texts' names

/// The name a token is, if an identifier.
fn name_of(t: &Token) -> Option<&str> {
    match t {
        Token::Word(w) | Token::Quoted(w) => Some(w),
        _ => None,
    }
}

/// `sql` with each token `pick` chooses replaced by `name` (quoted as
/// it must be).
fn replace(
    sql: &str,
    name: &str,
    pick: impl Fn(&[(Token, std::ops::Range<usize>)], usize) -> bool,
) -> Result<String> {
    let toks = tokenize_spans(sql)?;
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for i in 0..toks.len() {
        if pick(&toks, i) {
            let span = &toks[i].1;
            out.push_str(&sql[at..span.start]);
            out.push_str(&crate::definitions::ident(name));
            at = span.end;
        }
    }
    out.push_str(&sql[at..]);
    Ok(out)
}

/// Whether token `i` is column `column` of table `table`: the name, not
/// called as a function, not a type after `::`, not a qualifier, not an
/// alias, and qualified (if it is) by the table's name.
fn is_column(
    toks: &[(Token, std::ops::Range<usize>)],
    i: usize,
    column: &str,
    table: &str,
) -> bool {
    if name_of(&toks[i].0) != Some(column) {
        return false;
    }
    if matches!(
        toks.get(i + 1).map(|t| &t.0),
        Some(Token::Op("(" | ".") | Token::Str(_))
    ) {
        return false;
    }
    // `extract(day from …)`'s field, `interval '1' day`'s unit: words of
    // the grammar there, not names.
    let extract_field =
        i >= 2 && matches!(toks[i - 1].0, Token::Op("(")) && toks[i - 2].0.is("extract");
    if extract_field || matches!(i.checked_sub(1).map(|p| &toks[p].0), Some(Token::Str(_))) {
        return false;
    }
    match i.checked_sub(1).map(|p| &toks[p].0) {
        Some(Token::Op("::")) => false,
        Some(t) if t.is("as") => false,
        Some(Token::Op(".")) => i >= 2 && name_of(&toks[i - 2].0) == Some(table),
        _ => true,
    }
}

/// Whether token `i` names table `table`: a qualifier (`t.c`), or what
/// `FROM` or `JOIN` reads.
fn is_table(toks: &[(Token, std::ops::Range<usize>)], i: usize, table: &str) -> bool {
    if name_of(&toks[i].0) != Some(table) {
        return false;
    }
    let after_dot = i > 0 && matches!(toks[i - 1].0, Token::Op("."));
    let qualifies = matches!(toks.get(i + 1).map(|t| &t.0), Some(Token::Op(".")));
    let read = i > 0 && (toks[i - 1].0.is("from") || toks[i - 1].0.is("join"));
    !after_dot && (qualifies || read)
}

/// `sql` with column `from` of `table` renamed `to`.
fn rename_column_in(sql: &str, table: &str, from: &str, to: &str) -> Result<String> {
    replace(sql, to, |toks, i| is_column(toks, i, from, table))
}

/// `sql` with table `from` renamed `to`.
fn rename_table_in(sql: &str, from: &str, to: &str) -> Result<String> {
    replace(sql, to, |toks, i| is_table(toks, i, from))
}

/// Whether `sql` may name column `column` of `table`.
fn names_column(sql: &str, table: &str, column: &str) -> bool {
    tokenize_spans(sql)
        .map(|toks| (0..toks.len()).any(|i| is_column(&toks, i, column, table)))
        .unwrap_or(true)
}

/// Whether `sql` may name table `table`.
fn names_table(sql: &str, table: &str) -> bool {
    tokenize_spans(sql)
        .map(|toks| (0..toks.len()).any(|i| is_table(&toks, i, table)))
        .unwrap_or(true)
}

/// The SQL texts a table keeps over its columns: its checks, its
/// generated columns' expressions, its index expressions.
fn texts_mut(t: &mut Table) -> Vec<&mut String> {
    let mut out: Vec<&mut String> = t.checks.iter_mut().map(|(_, s)| s).collect();
    for c in &mut t.columns {
        match &mut c.generated {
            Some(crate::ast::Generated::Stored(s)) => out.push(s),
            Some(crate::ast::Generated::Embedding { source, .. }) => out.push(source),
            None => {}
        }
    }
    for ix in &mut t.indexes {
        for e in ix.exprs.iter_mut().flatten() {
            out.push(&mut e.sql);
        }
    }
    out
}

/// The columns each of `t`'s checks and generated columns reads, in
/// [`texts_mut`]'s order (index expressions keep theirs).
fn reads(txn: &dyn KvTxn, reg: &Registry, t: &Table) -> Result<Vec<Vec<usize>>> {
    let scope = crate::dml::table_scope(t);
    let mut binder = crate::binder::Binder::new(txn, reg);
    let mut out = Vec::new();
    let mut push = |e: crate::expr::Expr| {
        let mut cols = e.columns();
        cols.sort_unstable();
        cols.dedup();
        out.push(cols);
    };
    for (_, sql) in &t.checks {
        push(binder.condition(&crate::parser::parse_expr(sql)?, &scope)?);
    }
    for c in &t.columns {
        if let Some(g) = &c.generated {
            push(
                binder
                    .expr(&crate::parser::parse_expr(g.expr())?, &scope)?
                    .0,
            );
        }
    }
    Ok(out)
}

/// A policy's texts mapped by `f`.
fn policy_texts(p: &Policy, f: impl Fn(&str) -> Result<String>) -> Result<Policy> {
    let mut q = p.clone();
    q.using = p.using.as_deref().map(&f).transpose()?;
    q.check = p.check.as_deref().map(&f).transpose()?;
    Ok(q)
}

/// The policies that may name column `column` of `t`: its own whose
/// texts name it, and other tables' whose texts read `t` and name it.
fn policies_naming(txn: &dyn KvTxn, t: &Table, column: &str) -> Result<Vec<Policy>> {
    let texts = |p: &Policy| p.using.iter().chain(&p.check).cloned().collect::<Vec<_>>();
    let mut out = Vec::new();
    for p in policies::all(txn)? {
        let names = texts(&p).iter().any(|s| {
            if p.table == t.name {
                names_column(s, &t.name, column)
            } else {
                names_table(s, &t.name)
                    && tokenize_spans(s).map_or(true, |toks| {
                        toks.iter().any(|(k, _)| name_of(k) == Some(column))
                    })
            }
        });
        if names {
            out.push(p);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ renames

/// `ALTER TABLE from RENAME TO to`.
fn rename_table(txn: &mut dyn KvTxn, reg: &Registry, from: &str, to: &str) -> Result<()> {
    let mut t = catalog::table(&*txn, from)?;
    if catalog::get_table(&*txn, to)?.is_some() {
        return Err(Error::duplicate_table(format!(
            "relation \"{to}\" already exists"
        )));
    }
    // A memory store's own indexes are named after it.
    if t.memory.is_some() {
        for ix in &mut t.indexes {
            for s in ["subject", "embedding", "content"] {
                if ix.name == format!("{from}_{s}") {
                    let name = format!("{to}_{s}");
                    if catalog::index_name_taken(&*txn, &name)? {
                        return Err(Error::duplicate_table(format!(
                            "relation \"{name}\" already exists"
                        )));
                    }
                    ix.name = name;
                }
            }
        }
    }
    for s in texts_mut(&mut t) {
        *s = rename_table_in(s, from, to)?;
    }
    for fk in &mut t.foreign_keys {
        if fk.table == from {
            fk.table = to.to_string();
        }
    }
    for (child, _) in &mut t.referenced_by {
        if child == from {
            *child = to.to_string();
        }
    }
    // Other tables' foreign keys to it and from it, its partitions.
    for mut o in catalog::tables(&*txn)? {
        if o.name == from {
            continue;
        }
        let before = o.clone();
        for fk in &mut o.foreign_keys {
            if fk.table == from {
                fk.table = to.to_string();
            }
        }
        for (child, _) in &mut o.referenced_by {
            if child == from {
                *child = to.to_string();
            }
        }
        if let Some((parent, _)) = &mut o.partition_of
            && parent == from
        {
            *parent = to.to_string();
        }
        if o != before {
            catalog::update_table(txn, &o)?;
        }
    }
    t.name = to.to_string();
    catalog::rename_table(txn, from, &t)?;
    crate::dml::Writer::new(&*txn, reg, t.clone())?;
    privileges::rename(txn, Kind::Table, from, to)?;
    for p in policies::all(&*txn)? {
        let mut q = policy_texts(&p, |s| rename_table_in(s, from, to))?;
        if q.table == from {
            q.table = to.to_string();
        }
        if q != p {
            policies::delete(txn, &p.table, &p.name)?;
            policies::put(txn, &q)?;
        }
    }
    crate::tokens::rename_table(txn, from, to)
}

/// `ALTER TABLE t RENAME COLUMN from TO to`, on one table.
fn rename_column(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    let old = catalog::table(&*txn, table)?;
    let c = column_of(&old, from)?;
    if old.column(to).is_some() {
        return Err(Error::duplicate_column(format!(
            "column \"{to}\" of relation \"{table}\" already exists"
        )));
    }
    refuse_kept(&old, c, "rename")?;
    // Other tables' policies reading this one and naming the column: their
    // aliases are theirs to know.
    if let Some(p) = policies_naming(&*txn, &old, from)?
        .into_iter()
        .find(|p| p.table != table)
    {
        return Err(Error::dependent_objects(format!(
            "cannot rename column \"{from}\" of table \"{table}\": policy \"{}\" on table \"{}\" may name it; drop it, rename, create it again",
            p.name, p.table
        )));
    }
    let mut new = old.clone();
    new.columns[c].name = to.to_string();
    for s in texts_mut(&mut new) {
        *s = rename_column_in(s, table, from, to)?;
    }
    // Each text reads what it read.
    if reads(&*txn, reg, &old)? != reads(&*txn, reg, &new)? {
        return Err(Error::feature(format!(
            "cannot rename column \"{from}\" of table \"{table}\": an expression naming it could not be rewritten; drop it first"
        )));
    }
    let shape = new.clone();
    for (ix, was) in new.indexes.iter_mut().zip(&old.indexes) {
        if ix.has_exprs() {
            crate::session::bind_index_exprs(&*txn, reg, &shape, ix)?;
            let same = ix.columns == was.columns
                && ix.exprs.iter().zip(&was.exprs).all(|(a, b)| {
                    a.as_ref().map(|e| (e.ty, &e.uses)) == b.as_ref().map(|e| (e.ty, &e.uses))
                });
            if !same {
                return Err(Error::feature(format!(
                    "cannot rename column \"{from}\" of table \"{table}\": index \"{}\"'s expression could not be rewritten; drop it first",
                    ix.name
                )));
            }
        }
    }
    crate::dml::Writer::new(&*txn, reg, new.clone())?;
    catalog::update_table(txn, &new)?;
    for p in policies::of(&*txn, table)? {
        let q = policy_texts(&p, |s| rename_column_in(s, table, from, to))?;
        if q != p {
            policies::put(txn, &q)?;
        }
    }
    privileges::rename_column(txn, table, from, Some(to))
}

/// `ALTER TABLE t RENAME CONSTRAINT from TO to`: a check, a foreign key,
/// or a unique index.
fn rename_constraint(txn: &mut dyn KvTxn, table: &str, from: &str, to: &str) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    let taken = t.checks.iter().any(|(n, _)| n == to)
        || t.foreign_keys.iter().any(|f| f.name == to)
        || t.indexes.iter().any(|i| i.name == to);
    if taken {
        return Err(Error::duplicate_object(format!(
            "constraint \"{to}\" for relation \"{table}\" already exists"
        )));
    }
    if let Some(check) = t.checks.iter_mut().find(|(n, _)| n == from) {
        check.0 = to.to_string();
        return catalog::update_table(txn, &t);
    }
    if let Some(i) = t.foreign_keys.iter().position(|f| f.name == from) {
        t.foreign_keys[i].name = to.to_string();
        if let Some(spec) = &mut t.chunks
            && spec.fk == from
        {
            spec.fk = to.to_string();
        }
        let by = (table.to_string(), from.to_string());
        let parent = t.foreign_keys[i].table.clone();
        if parent == table {
            for b in t.referenced_by.iter_mut().filter(|b| **b == by) {
                b.1 = to.to_string();
            }
        } else {
            let mut p = catalog::table(&*txn, &parent)?;
            for b in p.referenced_by.iter_mut().filter(|b| **b == by) {
                b.1 = to.to_string();
            }
            catalog::update_table(txn, &p)?;
        }
        return catalog::update_table(txn, &t);
    }
    if t.indexes.iter().any(|i| i.name == from && i.unique) {
        return rename_index(txn, from, to, false);
    }
    Err(Error::undefined_object(format!(
        "constraint \"{from}\" of relation \"{table}\" does not exist"
    )))
}

/// `ALTER INDEX name RENAME TO to`.
pub(crate) fn rename_index(
    txn: &mut dyn KvTxn,
    name: &str,
    to: &str,
    if_exists: bool,
) -> Result<()> {
    let Some((mut t, i)) = catalog::find_index(&*txn, name)? else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "index \"{name}\" does not exist"
        )));
    };
    if catalog::index_name_taken(&*txn, to)? {
        return Err(Error::duplicate_table(format!(
            "relation \"{to}\" already exists"
        )));
    }
    if t.memory.is_some()
        && ["subject", "embedding", "content"]
            .iter()
            .any(|s| name == format!("{}_{s}", t.name))
    {
        return Err(Error::wrong_object_type(format!(
            "cannot rename index \"{name}\": memory store \"{}\" keeps it",
            t.name
        )));
    }
    t.indexes[i].name = to.to_string();
    catalog::update_table(txn, &t)
}

// --------------------------------------------------------- drop column

/// `ALTER TABLE t DROP COLUMN column [CASCADE]`, on one table.
fn drop_column(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    cascade: bool,
) -> Result<()> {
    let old = catalog::table(&*txn, table)?;
    // Dropped where it is stored, no row read or written, or the rows
    // rewritten without it — read, a premise of the commit: a write
    // committed meanwhile was made under the old definition.
    let in_place = drops_in_place(&old);
    if !in_place {
        crate::dml::rows_are_premise(&*txn, &old)?;
    }
    let c = column_of(&old, column)?;
    refuse_kept(&old, c, "drop")?;
    let depends = |what: String| -> Result<()> {
        Err(Error::dependent_objects(format!(
            "cannot drop column \"{column}\" of table \"{table}\": {what}"
        )))
    };
    if old.columns.len() == 1 {
        return depends("it is the table's only column; drop the table".into());
    }
    if old.primary_key.contains(&c) {
        return depends("it is in the primary key".into());
    }
    if old
        .partition_by
        .as_ref()
        .is_some_and(|s| s.columns.contains(&c))
    {
        return depends("it is in the partition key".into());
    }
    if old.subject.contains(&c) {
        return depends(format!(
            "it is a subject column; ALTER TABLE {table} SET SUBJECT without it first"
        ));
    }
    if old.expiry == Some(c) {
        return depends(format!(
            "it is the expiry column; ALTER TABLE {table} DROP EXPIRY first"
        ));
    }
    if old.copy != crate::column_store::CopyState::Ready {
        return Err(Error::object_in_use(format!(
            "table \"{table}\"'s storage is being converted; drop the column when it is done"
        )));
    }
    if old.columnar == [c] {
        return depends("it is all the column copy holds; SET STORAGE first".into());
    }
    let read = reads(&*txn, reg, &old)?;
    let checks = old.checks.len();
    for (k, g) in old
        .columns
        .iter()
        .enumerate()
        .filter(|(_, col)| col.generated.is_some())
        .enumerate()
    {
        if read[checks + k].contains(&c) {
            return depends(format!(
                "generated column \"{}\" is made from it; drop that first",
                g.1.name
            ));
        }
    }
    // Other tables' foreign keys to it, and chunk tables of its text.
    let mut children = Vec::new();
    for (child, fk) in &old.referenced_by {
        if child == table {
            continue;
        }
        let ct = catalog::table(&*txn, child)?;
        if ct
            .chunks
            .as_ref()
            .is_some_and(|s| s.fk == *fk && s.column == c)
        {
            return depends(format!(
                "chunk table \"{child}\" holds its chunks; drop that first"
            ));
        }
        if ct
            .foreign_keys
            .iter()
            .any(|f| f.name == *fk && f.ref_columns.contains(&c))
        {
            children.push((child.clone(), fk.clone()));
        }
    }
    let named_by = policies_naming(&*txn, &old, column)?;
    if !cascade {
        if let Some((child, fk)) = children.first() {
            return Err(Error::dependent_objects(format!(
                "cannot drop column \"{column}\" of table \"{table}\": constraint \"{fk}\" on table \"{child}\" depends on it"
            ))
            .with_hint("DROP COLUMN … CASCADE drops it too"));
        }
        if let Some(p) = named_by.first() {
            return Err(Error::dependent_objects(format!(
                "cannot drop column \"{column}\" of table \"{table}\": policy \"{}\" on table \"{}\" may name it",
                p.name, p.table
            ))
            .with_hint("DROP COLUMN … CASCADE drops it too"));
        }
    }
    let mut new = old.clone();
    for (child, fk) in &children {
        let mut ct = catalog::table(&*txn, child)?;
        ct.foreign_keys.retain(|f| f.name != *fk);
        catalog::update_table(txn, &ct)?;
        new.referenced_by.retain(|b| b.0 != *child || b.1 != *fk);
    }
    for p in &named_by {
        policies::delete(txn, &p.table, &p.name)?;
    }
    // Its own foreign keys over it (and self-references to it).
    let mut i = 0;
    while i < new.foreign_keys.len() {
        let f = &new.foreign_keys[i];
        if f.columns.contains(&c) || (f.table == new.name && f.ref_columns.contains(&c)) {
            crate::fk::detach(txn, &mut new, i)?;
        } else {
            i += 1;
        }
    }
    if in_place {
        // Its indexes' entries removed after the drop commits (the rows
        // are not rewritten, which would remove them).
        for ix in new.indexes.iter().filter(|ix| ix.uses(c)) {
            new.dropped.push(ix.id);
        }
        if new.indexes.iter().any(|ix| ix.uses(c)) {
            crate::online_index::mark_dropped();
        }
    }
    new.indexes.retain(|ix| !ix.uses(c));
    new.checks = new
        .checks
        .iter()
        .zip(&read)
        .filter(|(_, r)| !r.contains(&c))
        .map(|(ch, _)| ch.clone())
        .collect();
    for s in &mut new.statistics {
        s.columns.retain(|&x| x != c);
    }
    new.statistics.retain(|s| s.columns.len() >= 2);
    new.columnar.retain(|&x| x != c);
    // Positions after it move one place down.
    let down = |p: &mut usize| {
        if *p > c {
            *p -= 1;
        }
    };
    new.primary_key.iter_mut().for_each(down);
    for ix in &mut new.indexes {
        ix.columns.iter_mut().for_each(down);
    }
    let me = new.name.clone();
    for f in &mut new.foreign_keys {
        f.columns.iter_mut().for_each(down);
        if f.table == me {
            f.ref_columns.iter_mut().for_each(down);
        }
    }
    for s in &mut new.statistics {
        s.columns.iter_mut().for_each(down);
    }
    new.columnar.iter_mut().for_each(down);
    if in_place {
        // Its place stays in the stored rows ([`Table::gone`]); the other
        // columns' places, the missing values' too, do not move.
        let at = old.stored_at(c);
        new.missing.retain(|(m, _)| *m != at);
        new.gone.push(at);
        new.gone.sort_unstable();
    } else {
        new.missing.retain(|(m, _)| *m != c);
        for (m, _) in &mut new.missing {
            down(m);
        }
    }
    new.subject.iter_mut().for_each(down);
    new.expiry.iter_mut().for_each(down);
    if let Some(spec) = &mut new.partition_by {
        spec.columns.iter_mut().for_each(down);
    }
    new.columns.remove(c);
    // Index expressions bound again (their columns found anew); methods'
    // indexes defined again over the new shape.
    let shape = new.clone();
    for ix in &mut new.indexes {
        if let Some(m) = crate::index_method::of(ix)? {
            m.define(&shape, ix)?;
        } else if ix.has_exprs() {
            crate::session::bind_index_exprs(&*txn, reg, &shape, ix)?;
        }
    }
    // Other tables' foreign keys to it, and its chunk tables.
    for (child, fk) in new.referenced_by.clone() {
        if child == me {
            continue;
        }
        let mut ct = catalog::table(&*txn, &child)?;
        for f in ct.foreign_keys.iter_mut().filter(|f| f.name == fk) {
            f.ref_columns.iter_mut().for_each(down);
        }
        if let Some(spec) = &mut ct.chunks
            && spec.fk == fk
        {
            down(&mut spec.column);
        }
        catalog::update_table(txn, &ct)?;
    }
    privileges::rename_column(txn, table, column, None)?;
    // The rows, without it (a partitioned table's are its partitions'),
    // written again whole.
    if !in_place && new.partition_by.is_none() {
        new.compact();
        let rows = {
            let ctx = Ctx::new(&*txn, reg, &[]);
            let mut rows = crate::spill::Stash::new();
            for r in crate::exec::matching(&old, &crate::plan::Access::Full, None, &ctx)? {
                let (key, mut row) = r?;
                if c < row.len() {
                    row.remove(c);
                }
                rows.push(crate::exec::keyed(key, row))?;
            }
            rows.seal()?
        };
        crate::dml::replace_rows(txn, reg, &new, rows)?;
    }
    crate::embedding::column_dropped(txn, new.id, c)?;
    txn.delete(&catalog::stats_key(&new))?;
    catalog::delete_stats_sample(txn, &new)?;
    catalog::update_table(txn, &new)
}

/// Whether a column of `t` is dropped where it is stored, no row read or
/// rewritten ([`Table::gone`]; issue #18): a table stored in rows, in
/// columns or in both (segments hold columns by their places in a stored
/// row too), not partitioned, nor a memory store or a chunk table, no
/// column's type being changed.
fn drops_in_place(t: &Table) -> bool {
    [
        crate::storage::ROW,
        crate::column_store::COLUMN,
        crate::column_store::ROW_COLUMN,
    ]
    .contains(&t.storage.as_str())
        && t.partition_by.is_none()
        && t.partition_of.is_none()
        && t.memory.is_none()
        && t.chunks.is_none()
        && t.converting.is_none()
}

// ------------------------------------------------ defaults, NOT NULL

/// `ALTER TABLE t ALTER COLUMN column SET DEFAULT sql | DROP DEFAULT`,
/// on one table.
fn set_default(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    default: Option<&str>,
) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    let c = column_of(&t, column)?;
    if t.columns[c].generated.is_some() {
        return Err(Error::invalid_object_definition(format!(
            "column \"{column}\" of table \"{table}\" is a generated column"
        )));
    }
    // Its default is its sequence's.
    if t.columns[c].identity.is_some() {
        return Err(Error::syntax(format!(
            "column \"{column}\" of relation \"{table}\" is an identity column"
        )));
    }
    refuse_kept(&t, c, "change the default of")?;
    t.columns[c].default = default.map(str::to_string);
    if default.is_some() {
        // Bound, and of the column's type.
        let w = crate::dml::Writer::new(&*txn, reg, t.clone())?;
        w.check_default(c, &Ctx::new(&*txn, reg, &[]))?;
    }
    catalog::update_table(txn, &t)
}

/// `ALTER TABLE t ALTER COLUMN column ADD GENERATED … AS IDENTITY | SET
/// GENERATED … | SET option | RESTART | DROP IDENTITY` (PostgreSQL's,
/// with its checks).
fn identity(
    txn: &mut dyn KvTxn,
    table: &str,
    column: &str,
    change: &crate::ast::IdentityChange,
) -> Result<()> {
    use crate::ast::IdentityChange as C;
    let mut t = catalog::table(&*txn, table)?;
    let c = column_of(&t, column)?;
    privileges::Checker::new().require_owner(&*txn, Kind::Table, table)?;
    if t.partition_by.is_some() || t.partition_of.is_some() {
        return Err(Error::feature(
            "identity columns of partitioned tables and partitions are not supported yet",
        ));
    }
    let not_identity = || {
        Error::prerequisite(format!(
            "column \"{column}\" of relation \"{table}\" is not an identity column"
        ))
    };
    match change {
        C::Add(auto) => {
            if t.columns[c].identity.is_some() {
                return Err(Error::prerequisite(format!(
                    "column \"{column}\" of relation \"{table}\" is already an identity column"
                )));
            }
            let ty = t.columns[c].ty;
            let identity = crate::sequences::auto_identity(&*txn, table, column, auto, ty)?;
            if t.columns[c].nullable && !t.primary_key.contains(&c) {
                return Err(Error::prerequisite(format!(
                    "column \"{column}\" of relation \"{table}\" must be declared NOT NULL before identity can be added"
                )));
            }
            if t.columns[c].default.is_some() || t.columns[c].generated.is_some() {
                return Err(Error::prerequisite(format!(
                    "column \"{column}\" of relation \"{table}\" already has a default value"
                )));
            }
            refuse_kept(&t, c, "change the default of")?;
            crate::sequences::make_auto(txn, table, column, auto, ty, &identity)?;
            t.columns[c].default = Some(crate::sequences::auto_default(&identity));
            t.columns[c].identity = Some(identity);
            catalog::update_table(txn, &t)
        }
        C::Set { always, options } => {
            let Some(identity) = t.columns[c].identity.clone() else {
                return Err(not_identity());
            };
            if options
                .iter()
                .any(|o| matches!(o, crate::sequences::SeqOption::OwnedBy(_)))
            {
                return Err(Error::syntax(
                    "an identity column's sequence is owned by it",
                ));
            }
            if let Some(a) = always
                && *a != identity.always
            {
                t.columns[c].identity = Some(crate::catalog::Identity {
                    always: *a,
                    ..identity.clone()
                });
                catalog::update_table(txn, &t)?;
            }
            if !options.is_empty() {
                crate::sequences::alter(
                    txn,
                    &identity.sequence,
                    &crate::sequences::AlterSequence::Options(options.clone()),
                    false,
                )?;
            }
            Ok(())
        }
        C::Drop { if_exists } => {
            let Some(identity) = t.columns[c].identity.take() else {
                return if *if_exists {
                    Ok(())
                } else {
                    Err(not_identity())
                };
            };
            t.columns[c].default = None;
            catalog::update_table(txn, &t)?;
            crate::sequences::remove(txn, &identity.sequence)
        }
    }
}

/// `ALTER TABLE t ALTER COLUMN column SET NOT NULL | DROP NOT NULL`, on
/// one table.
fn set_not_null(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    not_null: bool,
) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    let c = column_of(&t, column)?;
    if !not_null && t.primary_key.contains(&c) {
        return Err(Error::invalid_table_definition(format!(
            "column \"{column}\" of table \"{table}\" is in a primary key"
        )));
    }
    refuse_kept(&t, c, "change")?;
    // Being checked online (its statement cut short): checked here, the
    // record cleared.
    if not_null
        && crate::validating::same_as_pending(
            &t,
            &crate::catalog::Validating::NotNull { column: c },
        )?
    {
        t.validating.clear();
    }
    if not_null && t.columns[c].nullable && t.partition_by.is_none() {
        crate::dml::rows_are_premise(&*txn, &t)?;
        let ctx = Ctx::new(&*txn, reg, &[]);
        for r in crate::exec::matching(&t, &crate::plan::Access::Full, None, &ctx)? {
            if r?.1.get(c).is_none_or(|v| v.is_null()) {
                return Err(Error::not_null_violation(format!(
                    "column \"{column}\" of relation \"{table}\" contains null values"
                )));
            }
        }
    }
    t.columns[c].nullable = !not_null;
    catalog::update_table(txn, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_columns_name_is_rewritten_where_it_is_the_column() {
        let r = |s: &str| rename_column_in(s, "t", "a", "b").unwrap();
        assert_eq!(r("a > 0 AND t.a < 10"), "b > 0 AND t.b < 10");
        // A function, a type, another table's column, a string: kept.
        assert_eq!(r("a(x) = 'a' AND u.a = 1"), "a(x) = 'a' AND u.a = 1");
        assert_eq!(r("x::a"), "x::a");
        let day = |s: &str| rename_column_in(s, "t", "day", "d").unwrap();
        assert_eq!(
            day("extract(day from at) > day"),
            "extract(day from at) > d"
        );
        assert_eq!(day("at + interval '1' day"), "at + interval '1' day");
        // A name that must be quoted is.
        assert_eq!(
            rename_column_in("lower(a)", "t", "a", "Big Name").unwrap(),
            "lower(\"Big Name\")"
        );
        assert_eq!(
            rename_column_in("\"Old\" + 1", "t", "Old", "new").unwrap(),
            "new + 1"
        );
    }

    #[test]
    fn a_tables_name_is_rewritten_where_it_is_the_table() {
        let r = |s: &str| rename_table_in(s, "t", "u").unwrap();
        assert_eq!(
            r("EXISTS (SELECT 1 FROM t WHERE t.x = owner)"),
            "EXISTS (SELECT 1 FROM u WHERE u.x = owner)"
        );
        // A column of the same name, kept.
        assert_eq!(r("t > 0 AND v.t = 1"), "t > 0 AND v.t = 1");
        assert!(names_table("x IN (SELECT y FROM t)", "t"));
        assert!(!names_table("t + 1", "t"));
    }
}
