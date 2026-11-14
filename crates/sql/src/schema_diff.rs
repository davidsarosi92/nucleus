//! The change from one schema to another (PLAN.md §M14.16 M4): the
//! statements that make database `from`'s schema `to`'s — what `nucleus
//! migrate diff` writes as a migration's `up` (and, the other way, its
//! `down`).
//!
//! Both schemas are compared as [`definitions`](crate::definitions) —
//! each object's creating statement, keyed by its kind, name and table —
//! and, for a table both have, as the catalog's structure: an object only
//! `from` has is dropped, one only `to` has is created, one that changed
//! is dropped and created again, except what can change in place — a
//! table's columns (`ADD`, `DROP`, `ALTER COLUMN … TYPE`, defaults,
//! `NOT NULL`) and constraints, an enum's new labels (`ADD VALUE`), an
//! ordering's mapping, a table's settings (storage, expiry, subject, row
//! security).  Drops come first (in the reverse of the definitions'
//! order, so what depends on something goes before it), then the tables'
//! changes, then what is created (in the definitions' order).
//!
//! What a diff cannot see or must not do is a note beside the
//! statements, not a statement: a column or table that may have been
//! renamed (a diff sees a drop and an add — Alembic's autogenerate says
//! the same), a primary key or partitioning changed, a partition, memory
//! store or chunk table whose statement changed (dropping it would lose
//! its rows), a column order that `ADD COLUMN` cannot give.
//!
//! **The order statements run in**, so each finds what it needs and
//! nothing still needs what it drops: what depends on tables is dropped
//! (foreign keys too, even of a table dropped after, so tables that
//! reference each other drop in any order), then the tables' columns and
//! constraints that go, then the types and models nothing uses any more;
//! then models and types are made, tables, the tables' new columns and
//! changes, and last what depends on them (indexes, foreign keys,
//! comments, settings, policies).

use std::collections::{HashMap, HashSet};

use nucleus_kv::KvTxn;

use crate::catalog::{self, ConstraintKind, Table};
use crate::definitions::{self, Definition, column_sql, ident, literal};
use crate::error::Result;
use crate::ncatalog::type_text;

/// The statements from one schema to another, and what they leave to a
/// person.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// The statements, in the order they run.
    pub statements: Vec<String>,
    /// What the statements do not do, or may do wrongly: each a sentence.
    pub notes: Vec<String>,
}

impl Diff {
    /// No statement and no note: the schemas are the same.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty() && self.notes.is_empty()
    }

    /// As a script: the notes as `--` comments, then each statement and
    /// its `;`.
    pub fn script(&self) -> String {
        let mut s = String::new();
        for n in &self.notes {
            s.push_str(&format!("-- {n}\n"));
        }
        for st in &self.statements {
            s.push_str(st);
            s.push_str(";\n");
        }
        s
    }
}

/// The kinds columns and tables are made of.
const BASE: [&str; 5] = ["model", "type", "ordering", "operator", "cast"];
/// The kinds that are tables.
const TABLES: [&str; 4] = ["table", "partition", "memory store", "chunk table"];
/// The kinds that set something on a table: changed, the new statement
/// is enough; gone, the setting goes back to its default.
const SETTINGS: [&str; 4] = ["storage", "expiry", "subject", "row security"];

type Key = (&'static str, String, Option<String>, String);

/// A definition's identity: kind, name, table — and, where a table has
/// several of a kind (its storage and storage policy, row security
/// enabled and forced), the statement's verb.
fn key(d: &Definition) -> Key {
    let which = match d.kind {
        "storage" if d.sql.contains("STORAGE POLICY") => "policy",
        "row security" if d.sql.ends_with("FORCE ROW LEVEL SECURITY") => "force",
        _ => "",
    };
    (d.kind, d.name.clone(), d.table.clone(), which.to_string())
}

/// The statements that make `from`'s schema `to`'s.
pub fn diff(from: &dyn KvTxn, to: &dyn KvTxn) -> Result<Diff> {
    let a = definitions::all(from)?;
    let b = definitions::all(to)?;
    let in_a: HashMap<Key, &Definition> = a.iter().map(|d| (key(d), d)).collect();
    let in_b: HashMap<Key, &Definition> = b.iter().map(|d| (key(d), d)).collect();
    let ta: HashMap<String, Table> = catalog::tables(from)?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let tb: HashMap<String, Table> = catalog::tables(to)?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let mut out = Diff::default();
    let (mut drops, mut base_drops) = (Vec::new(), Vec::new());
    let (mut removals, mut additions) = (Vec::new(), Vec::new());
    let (mut base, mut tables, mut rest) = (Vec::new(), Vec::new(), Vec::new());
    // Definitions that changed and were dealt with in place.
    let mut in_place: HashSet<Key> = HashSet::new();

    let gone: HashSet<&str> = a
        .iter()
        .filter(|d| TABLES.contains(&d.kind) && !in_b.contains_key(&key(d)))
        .map(|d| d.name.as_str())
        .collect();
    // What goes with a table dropped: its own definitions but foreign
    // keys, its partitions.
    let goes_with_table = |d: &Definition| {
        let table = d.table.as_deref().unwrap_or("");
        if TABLES.contains(&d.kind) {
            ta.get(&d.name)
                .and_then(|t| t.partition_of.as_ref())
                .is_some_and(|(p, _)| gone.contains(p.as_str()))
        } else {
            d.kind != "foreign key" && gone.contains(table)
        }
    };

    for d in a.iter().rev() {
        let k = key(d);
        match in_b.get(&k) {
            Some(e) if e.sql == d.sql => continue,
            Some(e) => {
                let done = match d.kind {
                    "table" => {
                        table_changes(
                            &ta[&d.name],
                            &tb[&d.name],
                            (&mut removals, &mut additions),
                            &mut out.notes,
                        );
                        true
                    }
                    "ordering" => {
                        base.push(e.sql.replacen(" ADD ORDERING ", " ALTER ORDERING ", 1));
                        true
                    }
                    "type" => enum_additions(from, to, &d.name, &mut base)?,
                    "partition" | "memory store" | "chunk table" => {
                        out.notes.push(format!(
                            "{} {} changed (from: {}; to: {}): dropping it would lose its rows, so the diff leaves it",
                            d.kind, d.name, d.sql, e.sql
                        ));
                        true
                    }
                    // Its new statement (below) is enough.
                    k if SETTINGS.contains(&k) => continue,
                    _ => false,
                };
                if done {
                    in_place.insert(k);
                    continue;
                }
            }
            None => {}
        }
        if goes_with_table(d) {
            continue;
        }
        if let Some(s) = drop_sql(d) {
            if BASE.contains(&d.kind) {
                base_drops.push(s);
            } else {
                drops.push(s);
            }
        }
    }
    for d in &b {
        let k = key(d);
        match in_a.get(&k) {
            Some(e) if e.sql == d.sql => {}
            Some(_) if in_place.contains(&k) => {}
            _ if BASE.contains(&d.kind) => base.push(d.sql.clone()),
            _ if TABLES.contains(&d.kind) => tables.push(d.sql.clone()),
            _ => rest.push(d.sql.clone()),
        }
    }
    rename_hints(&a, &b, &in_a, &in_b, &mut out.notes);
    out.statements = [drops, removals, base_drops, base, tables, additions, rest].concat();
    Ok(out)
}

/// [`diff`] of two databases' current schemas.
pub fn between(from: &dyn nucleus_kv::Kv, to: &dyn nucleus_kv::Kv) -> Result<Diff> {
    let a = from.begin(nucleus_kv::TxnOptions::read_only())?;
    let b = to.begin(nucleus_kv::TxnOptions::read_only())?;
    diff(&*a, &*b)
}

/// The statement that undoes definition `d`.
fn drop_sql(d: &Definition) -> Option<String> {
    let table = d.table.as_deref().map(ident).unwrap_or_default();
    let alter = |what: &str| Some(format!("ALTER TABLE {table} {what}"));
    let name = ident(&d.name);
    match d.kind {
        "model" => Some(format!("DROP MODEL {name}")),
        "type" => Some(format!("DROP TYPE {name}")),
        "ordering" => d
            .sql
            .split_once(" ADD ORDERING ")
            .map(|(head, _)| format!("{head} DROP ORDERING {name}")),
        "operator" | "cast" => d
            .sql
            .rsplit_once(" AS ")
            .map(|(head, _)| head.replacen(" ADD ", " DROP ", 1)),
        "table" | "partition" | "memory store" | "chunk table" => {
            Some(format!("DROP TABLE {name}"))
        }
        "column" => alter(&format!("DROP COLUMN {name}")),
        "constraint" | "foreign key" => alter(&format!("DROP CONSTRAINT {name}")),
        "index" => Some(format!("DROP INDEX {name}")),
        "statistics" => Some(format!("DROP STATISTICS {name}")),
        "comment" => d
            .sql
            .find(" IS ")
            .map(|i| format!("{} IS NULL", &d.sql[..i])),
        "storage" if d.sql.contains("STORAGE POLICY") => alter("SET STORAGE POLICY NONE"),
        "storage" => alter("SET STORAGE row"),
        "expiry" if d.sql.ends_with("DROP EXPIRY") => alter("SET EXPIRY (expires_at)"),
        "expiry" => alter("DROP EXPIRY"),
        "subject" if d.sql.ends_with("DROP SUBJECT") => alter("SET SUBJECT (subject)"),
        "subject" => alter("DROP SUBJECT"),
        "row security" if d.sql.ends_with("FORCE ROW LEVEL SECURITY") => {
            alter("NO FORCE ROW LEVEL SECURITY")
        }
        "row security" => alter("DISABLE ROW LEVEL SECURITY"),
        "policy" => Some(format!("DROP POLICY {name} ON {table}")),
        _ => None,
    }
}

/// The changes to a table both schemas have: its constraints and
/// columns — what goes (`removals`), what comes and changes.
fn table_changes(
    a: &Table,
    b: &Table,
    (removals, out): (&mut Vec<String>, &mut Vec<String>),
    notes: &mut Vec<String>,
) {
    let t = ident(&b.name);
    let names = |t: &Table, cols: &[usize]| -> Vec<String> {
        cols.iter().map(|&c| t.columns[c].name.clone()).collect()
    };
    if names(a, &a.primary_key) != names(b, &b.primary_key) {
        notes.push(format!(
            "the primary key of {} changes ({:?} to {:?}): make a new table and copy the rows",
            b.name,
            names(a, &a.primary_key),
            names(b, &b.primary_key)
        ));
    }
    let partitioning = |t: &Table| {
        t.partition_by
            .as_ref()
            .map(|s| (format!("{:?}", s.kind), names(t, &s.columns)))
    };
    if partitioning(a) != partitioning(b) {
        notes.push(format!(
            "the partitioning of {} changes: make a new table and copy the rows",
            b.name
        ));
    }
    let plain = |s: &str| s != crate::column_store::ROW_COLUMN;
    if a.storage != b.storage && plain(&b.storage) {
        out.push(format!("ALTER TABLE {t} SET STORAGE {}", ident(&b.storage)));
    }
    // Constraints but the primary key and foreign keys (their own kinds).
    let constraints = |t: &Table| -> Vec<(String, String)> {
        catalog::constraints(std::slice::from_ref(t))
            .into_iter()
            .filter(|c| matches!(c.kind, ConstraintKind::Unique(_) | ConstraintKind::Check(_)))
            .map(|c| (c.name, c.definition))
            .collect()
    };
    let (ca, cb) = (constraints(a), constraints(b));
    for c in ca.iter().filter(|c| !cb.contains(c)) {
        removals.push(format!("ALTER TABLE {t} DROP CONSTRAINT {}", ident(&c.0)));
    }
    let added: Vec<_> = b
        .columns
        .iter()
        .filter(|c| a.column(&c.name).is_none())
        .collect();
    let dropped: Vec<_> = a
        .columns
        .iter()
        .filter(|c| b.column(&c.name).is_none())
        .collect();
    for c in &dropped {
        removals.push(format!("ALTER TABLE {t} DROP COLUMN {}", ident(&c.name)));
    }
    for c in &added {
        out.push(format!("ALTER TABLE {t} ADD COLUMN {}", column_sql(c)));
    }
    for cb in &b.columns {
        let Some(ca) = a.column(&cb.name).map(|i| &a.columns[i]) else {
            continue;
        };
        let col = ident(&cb.name);
        let alter = |what: String| format!("ALTER TABLE {t} ALTER COLUMN {col} {what}");
        if ca.generated != cb.generated {
            out.push(format!("ALTER TABLE {t} DROP COLUMN {col}"));
            out.push(format!("ALTER TABLE {t} ADD COLUMN {}", column_sql(cb)));
            continue;
        }
        if type_text(ca) != type_text(cb) {
            out.push(alter(format!("TYPE {}", type_text(cb))));
        }
        if ca.default != cb.default {
            out.push(alter(match &cb.default {
                Some(d) => format!("SET DEFAULT {d}"),
                None => "DROP DEFAULT".into(),
            }));
        }
        if ca.nullable != cb.nullable {
            out.push(alter(
                if cb.nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                }
                .into(),
            ));
        }
    }
    // The order the columns end in: those both have in `a`'s order, the
    // added ones after.
    let kept: Vec<&str> = a
        .columns
        .iter()
        .filter(|c| b.column(&c.name).is_some())
        .map(|c| c.name.as_str())
        .chain(added.iter().map(|c| c.name.as_str()))
        .collect();
    let wanted: Vec<&str> = b.columns.iter().map(|c| c.name.as_str()).collect();
    if kept != wanted {
        notes.push(format!(
            "the columns of {} end in the order {}, not {}: ADD COLUMN adds at the end",
            b.name,
            kept.join(", "),
            wanted.join(", ")
        ));
    }
    for (da, db) in dropped.iter().zip(&added) {
        if dropped.len() == 1 && added.len() == 1 && type_text(da) == type_text(db) {
            notes.push(format!(
                "{}.{} dropped and {}.{} added: if it is a rename, ALTER TABLE {t} RENAME COLUMN {} TO {} keeps its values",
                b.name,
                da.name,
                b.name,
                db.name,
                ident(&da.name),
                ident(&db.name)
            ));
        }
    }
    for c in cb.iter().filter(|c| !ca.contains(c)) {
        out.push(format!(
            "ALTER TABLE {t} ADD CONSTRAINT {} {}",
            ident(&c.0),
            c.1
        ));
    }
}

/// An enum whose labels `to` extends in order: `ADD VALUE` for each new
/// one (from the last, so each has its place's neighbour).  `false`: not
/// so (a label gone or moved) — dropped and made again.
fn enum_additions(
    from: &dyn KvTxn,
    to: &dyn KvTxn,
    name: &str,
    out: &mut Vec<String>,
) -> Result<bool> {
    let labels = |txn: &dyn KvTxn| -> Result<Vec<String>> {
        Ok(crate::enums::get(txn, name)?
            .map(|e| e.labels.into_iter().map(|(_, l)| l).collect())
            .unwrap_or_default())
    };
    let (la, lb) = (labels(from)?, labels(to)?);
    let kept: Vec<&String> = lb.iter().filter(|l| la.contains(l)).collect();
    if kept.len() != la.len() || kept.iter().zip(&la).any(|(k, l)| *k != l) {
        return Ok(false);
    }
    for (i, l) in lb.iter().enumerate().rev() {
        if la.contains(l) {
            continue;
        }
        let place = match lb.get(i + 1) {
            Some(next) => format!(" BEFORE {}", literal(next)),
            None => String::new(),
        };
        out.push(format!(
            "ALTER TYPE {} ADD VALUE {}{place}",
            ident(name),
            literal(l)
        ));
    }
    Ok(true)
}

/// A table dropped and one created with the same columns: maybe a
/// rename.
fn rename_hints(
    a: &[Definition],
    b: &[Definition],
    in_a: &HashMap<Key, &Definition>,
    in_b: &HashMap<Key, &Definition>,
    notes: &mut Vec<String>,
) {
    // Its columns and constraints, the names it gives them (`t_pkey`)
    // without its own.
    let body = |d: &Definition| {
        d.sql
            .split_once(" (")
            .map(|(_, rest)| rest.replace(&format!("{}_", d.name), "_"))
            .unwrap_or_default()
    };
    let gone: Vec<_> = a
        .iter()
        .filter(|d| d.kind == "table" && !in_b.contains_key(&key(d)))
        .collect();
    let new: Vec<_> = b
        .iter()
        .filter(|d| d.kind == "table" && !in_a.contains_key(&key(d)))
        .collect();
    for g in &gone {
        for n in new.iter().filter(|n| body(n) == body(g)) {
            notes.push(format!(
                "table {} dropped and {} created with its columns: if it is a rename, ALTER TABLE {} RENAME TO {} keeps its rows",
                g.name,
                n.name,
                ident(&g.name),
                ident(&n.name)
            ));
        }
    }
}
