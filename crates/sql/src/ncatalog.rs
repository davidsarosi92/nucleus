//! `nc_catalog`: nucleus's own system catalog, as read-only tables
//! (PLAN.md §M14.5, D13).
//!
//! It describes the database in nucleus's terms — table and index ids,
//! the key prefixes their entries live under, foreign key actions,
//! statistics — and is what nucleus's own tools and documentation use.
//! Its views are registered by the built-in module ([`crate::builtin`]);
//! `pg_catalog` / `information_schema` are another module's, for
//! PostgreSQL clients ([`crate::pgcatalog`]).
//!
//! | table | one row per |
//! |-------|-------------|
//! | `nc_catalog.schemas` | schema (of the session's registry) |
//! | `nc_catalog.tables` | table |
//! | `nc_catalog.columns` | column |
//! | `nc_catalog.indexes` | index (the primary index is index 0) |
//! | `nc_catalog.constraints` | primary key, unique, check, foreign key |
//! | `nc_catalog.foreign_keys` | foreign key |
//! | `nc_catalog.statistics` | analyzed column (`ANALYZE`) |
//! | `nc_catalog.operators` | operator outside the grammar's own: a type's distance, a module's, or taught (`ALTER TYPE … ADD OPERATOR`) |
//! | `nc_catalog.casts` | cast taught (`ALTER TYPE … ADD CAST`) |
//! | `nc_catalog.orderings` | ordering taught to a type (`ALTER TYPE … ADD ORDERING`) |
//! | `nc_catalog.types` | type (of the session's registry), with its abilities |

use std::sync::Arc;

use nucleus_kv::KvTxn;

use crate::catalog::{self, Column, ConstraintKind, Table};
use crate::error::{Error, Result};
use crate::ext::{CallContext, CatalogView, Registry};
use crate::stats::Stats;
use crate::types::{DataType, Row, Value};

const B: DataType = DataType::Bool;
const I: DataType = DataType::BigInt;
const T: DataType = DataType::Text;
const TA: DataType = DataType::Array(&DataType::Text);

const DEFS: &[(&str, &[(&str, DataType)])] = &[
    ("schemas", &[("name", T), ("description", T)]),
    (
        "tables",
        &[
            ("table_id", I),
            ("name", T),
            ("columns", I),
            ("primary_key", TA),
            ("has_row_id", B),
            ("indexes", I),
            ("checks", I),
            ("foreign_keys", I),
            ("referenced_by", I),
            ("key_prefix", T),
            ("analyzed_rows", I),
        ],
    ),
    (
        "columns",
        &[
            ("table_id", I),
            ("table_name", T),
            ("position", I),
            ("name", T),
            ("type", T),
            ("nullable", B),
            ("default_expr", T),
            ("in_primary_key", B),
        ],
    ),
    (
        "indexes",
        &[
            ("table_id", I),
            ("table_name", T),
            ("index_id", I),
            ("name", T),
            ("columns", TA),
            ("is_unique", B),
            ("is_primary", B),
            ("key_prefix", T),
            ("orderings", TA),
        ],
    ),
    (
        "constraints",
        &[
            ("table_id", I),
            ("table_name", T),
            ("name", T),
            ("kind", T),
            ("definition", T),
        ],
    ),
    (
        "foreign_keys",
        &[
            ("table_id", I),
            ("table_name", T),
            ("name", T),
            ("columns", TA),
            ("referenced_table", T),
            ("referenced_columns", TA),
            ("on_delete", T),
            ("on_update", T),
        ],
    ),
    (
        "statistics",
        &[
            ("table_id", I),
            ("table_name", T),
            ("column_name", T),
            ("rows", I),
            ("nulls", I),
            ("distinct_values", I),
            ("histogram", TA),
        ],
    ),
    (
        "operators",
        &[
            ("symbol", T),
            ("left_type", T),
            ("right_type", T),
            ("function", T),
            ("source", T),
        ],
    ),
    (
        "casts",
        &[
            ("from_type", T),
            ("to_type", T),
            ("function", T),
            ("context", T),
        ],
    ),
    (
        "orderings",
        &[("type", T), ("name", T), ("mapping", T), ("used_by", TA)],
    ),
    (
        "enums",
        &[("type", T), ("label", T), ("position", I), ("sort_key", T)],
    ),
    (
        "types",
        &[
            ("name", T),
            ("aliases", TA),
            ("category", T),
            ("module", T),
            ("equality", B),
            ("hashing", B),
            ("ordering", B),
            ("key_encoding", B),
            ("distance", T),
            ("encoding_version", I),
            ("stored_version", I),
        ],
    ),
];

/// An `nc_catalog` view.
struct NcView {
    name: &'static str,
    columns: &'static [(&'static str, DataType)],
}

impl CatalogView for NcView {
    fn schema(&self) -> &str {
        "nc_catalog"
    }

    fn name(&self) -> &str {
        self.name
    }

    fn columns(&self) -> Vec<(String, DataType)> {
        self.columns
            .iter()
            .map(|(n, t)| ((*n).to_string(), *t))
            .collect()
    }

    fn rows(&self, cx: &CallContext<'_>) -> Result<Vec<Row>> {
        rows(self.name, cx.txn(), cx.registry())
    }
}

/// Every `nc_catalog` view.
pub fn views() -> Vec<Arc<dyn CatalogView>> {
    DEFS.iter()
        .map(|(name, columns)| Arc::new(NcView { name, columns }) as Arc<dyn CatalogView>)
        .collect()
}

/// A column's type as nucleus writes it.
pub fn type_text(c: &Column) -> String {
    match (c.ty, c.max_len, c.numeric) {
        (DataType::Text, Some(n), _) => format!("varchar({n})"),
        (DataType::Char, Some(n), _) => format!("char({n})"),
        (DataType::Numeric, _, Some((p, s))) => format!("numeric({p},{s})"),
        (DataType::Timestamp, ..) => "timestamp".into(),
        (DataType::TimestampTz, ..) => "timestamptz".into(),
        (t, ..) => t.name().into(),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

fn names(t: &Table, cols: &[usize]) -> Value {
    Value::Array(
        cols.iter()
            .map(|&c| text(t.columns[c].name.clone()))
            .collect(),
    )
}

fn id(t: &Table) -> Value {
    Value::Int(i64::from(t.id))
}

/// The rows of `nc_catalog.<name>`.
fn rows(name: &str, txn: &dyn KvTxn, registry: &Registry) -> Result<Vec<Row>> {
    let tables = catalog::tables(txn)?;
    let mut out: Vec<Row> = Vec::new();
    match name {
        "schemas" => {
            for s in registry.schemas() {
                out.push(vec![text(s.name.clone()), text(s.description.clone())]);
            }
        }
        "tables" => {
            let stats = Stats::new(txn);
            for t in &tables {
                out.push(vec![
                    id(t),
                    text(t.name.clone()),
                    Value::Int(t.columns.len() as i64),
                    names(t, &t.primary_key),
                    Value::Bool(t.primary_key.is_empty()),
                    Value::Int(t.indexes.len() as i64),
                    Value::Int(t.checks.len() as i64),
                    Value::Int(t.foreign_keys.len() as i64),
                    Value::Int(t.referenced_by.len() as i64),
                    text(hex(&t.data_prefix())),
                    stats
                        .of(t)
                        .map_or(Value::Null, |s| Value::Int(s.rows as i64)),
                ]);
            }
        }
        "columns" => {
            for t in &tables {
                for (c, col) in t.columns.iter().enumerate() {
                    let in_pk = t.primary_key.contains(&c);
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(c as i64 + 1),
                        text(col.name.clone()),
                        text(type_text(col)),
                        Value::Bool(col.nullable && !in_pk),
                        col.default.clone().map_or(Value::Null, text),
                        Value::Bool(in_pk),
                    ]);
                }
            }
        }
        "indexes" => {
            for t in &tables {
                if !t.primary_key.is_empty() {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(0),
                        text(format!("{}_pkey", t.name)),
                        names(t, &t.primary_key),
                        Value::Bool(true),
                        Value::Bool(true),
                        text(hex(&t.index_prefix(0))),
                        Value::Array(vec![Value::Null; t.primary_key.len()]),
                    ]);
                }
                for ix in &t.indexes {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(i64::from(ix.id)),
                        text(ix.name.clone()),
                        names(t, &ix.columns),
                        Value::Bool(ix.unique),
                        Value::Bool(false),
                        text(hex(&t.index_prefix(ix.id))),
                        Value::Array(
                            ix.orderings
                                .iter()
                                .map(|o| o.clone().map_or(Value::Null, text))
                                .collect(),
                        ),
                    ]);
                }
            }
        }
        "constraints" => {
            for c in catalog::constraints(&tables) {
                let kind = match c.kind {
                    ConstraintKind::PrimaryKey => "primary key",
                    ConstraintKind::Unique(_) => "unique",
                    ConstraintKind::Check(_) => "check",
                    ConstraintKind::ForeignKey(_) => "foreign key",
                };
                out.push(vec![
                    id(c.table),
                    text(c.table.name.clone()),
                    text(c.name),
                    text(kind),
                    text(c.definition),
                ]);
            }
        }
        "foreign_keys" => {
            for t in &tables {
                for fk in &t.foreign_keys {
                    let parent = tables.iter().find(|p| p.name == fk.table);
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        text(fk.name.clone()),
                        names(t, &fk.columns),
                        text(fk.table.clone()),
                        parent.map_or(Value::Null, |p| names(p, &fk.ref_columns)),
                        text(fk.on_delete.sql().to_lowercase()),
                        text(fk.on_update.sql().to_lowercase()),
                    ]);
                }
            }
        }
        "statistics" => {
            let stats = Stats::new(txn);
            for t in &tables {
                let Some(s) = stats.of(t) else { continue };
                for (c, cs) in s.columns.iter().enumerate() {
                    let Some(col) = t.columns.get(c) else {
                        continue;
                    };
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        text(col.name.clone()),
                        Value::Int(s.rows as i64),
                        Value::Int(cs.nulls as i64),
                        Value::Int(cs.distinct as i64),
                        Value::Array(cs.bounds.iter().map(|b| text(b.render())).collect()),
                    ]);
                }
            }
        }
        "operators" => {
            let name = |t: DataType| text(crate::builtin::type_of(t).name());
            for (ty, _) in registry.types() {
                if let Some(d) = crate::builtin::type_of(*ty).distance() {
                    let f = crate::ext::DistanceFn::of(*ty);
                    out.push(vec![
                        text(d.operator()),
                        name(*ty),
                        name(*ty),
                        text(f.name()),
                        text("distance"),
                    ]);
                }
            }
            for (symbol, l, r, f) in registry.operators() {
                out.push(vec![
                    text(symbol),
                    name(l),
                    name(r),
                    text(f.name()),
                    text("registered"),
                ]);
            }
            for o in catalog::operators(txn)? {
                out.push(vec![
                    text(o.symbol),
                    text(o.left),
                    text(o.right),
                    text(o.function),
                    text("taught"),
                ]);
            }
        }
        "enums" => {
            for e in crate::enums::all(txn)? {
                for (i, (key, label)) in e.labels.iter().enumerate() {
                    out.push(vec![
                        text(e.name.clone()),
                        text(label.clone()),
                        Value::Int(i as i64 + 1),
                        text(hex(key)),
                    ]);
                }
            }
        }
        "casts" => {
            for c in catalog::casts(txn)? {
                out.push(vec![
                    text(c.from),
                    text(c.to),
                    text(c.function),
                    text(c.context.sql()),
                ]);
            }
        }
        "orderings" => {
            for o in catalog::orderings(txn)? {
                let mut users = Vec::new();
                for t in &tables {
                    for ix in &t.indexes {
                        let uses = ix.columns.iter().zip(&ix.orderings).any(|(&c, u)| {
                            u.as_deref() == Some(o.name.as_str())
                                && crate::builtin::type_of(t.columns[c].ty).name() == o.type_name
                        });
                        if uses {
                            users.push(text(ix.name.clone()));
                        }
                    }
                }
                out.push(vec![
                    text(o.type_name),
                    text(o.name),
                    text(o.mapping),
                    Value::Array(users),
                ]);
            }
        }
        "types" => {
            for (ty, module) in registry.types() {
                let ty = *ty;
                let t = crate::builtin::type_of(ty);
                let mut aliases: Vec<Value> = t.aliases().iter().map(|a| text(*a)).collect();
                aliases.extend(
                    registry
                        .type_aliases(ty)
                        .into_iter()
                        .filter(|a| a != t.name() && !t.aliases().contains(&a.as_str()))
                        .map(text),
                );
                out.push(vec![
                    text(t.name()),
                    Value::Array(aliases),
                    text(t.category()),
                    text(module.clone()),
                    Value::Bool(t.equality().is_some()),
                    Value::Bool(t.hashing().is_some()),
                    Value::Bool(t.ordering().is_some()),
                    Value::Bool(t.key_encoding().is_some()),
                    t.distance().map_or(Value::Null, |d| text(d.operator())),
                    Value::Int(i64::from(t.encoding_version())),
                    match ty {
                        DataType::Ext(r) => catalog::stored_version(txn, r.name())?
                            .map_or(Value::Null, |v| Value::Int(i64::from(v))),
                        _ => Value::Null,
                    },
                ]);
            }
        }
        _ => return Err(Error::internal(format!("no nc_catalog table {name}"))),
    }
    Ok(out)
}
