//! `nc_catalog`: nucleus's own system catalog, as read-only tables
//! (PLAN.md §M14.5, D13).
//!
//! It describes the database in nucleus's terms — table and index ids,
//! the key prefixes their entries live under, foreign key actions,
//! statistics — and is what nucleus's own tools and documentation use.
//! (`pg_catalog` / `information_schema` are a compatibility layer for
//! PostgreSQL clients, built from the same catalog; see
//! [`crate::pgcatalog`].)
//!
//! | table | one row per |
//! |-------|-------------|
//! | `nc_catalog.schemas` | schema |
//! | `nc_catalog.tables` | table |
//! | `nc_catalog.columns` | column |
//! | `nc_catalog.indexes` | index (the primary index is index 0) |
//! | `nc_catalog.constraints` | primary key, unique, check, foreign key |
//! | `nc_catalog.foreign_keys` | foreign key |
//! | `nc_catalog.statistics` | analyzed column (`ANALYZE`) |
//! | `nc_catalog.types` | type |

use nucleus_kv::KvTxn;

use crate::catalog::{self, Column, Table};
use crate::error::{Error, Result};
use crate::stats::Stats;
use crate::types::{DataType, Row, Value};

const B: DataType = DataType::Bool;
const I: DataType = DataType::Int;
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
    ("types", &[("name", T), ("aliases", TA), ("category", T)]),
];

fn columns_of(cols: &[(&str, DataType)]) -> Vec<Column> {
    cols.iter()
        .map(|(n, ty)| Column {
            name: (*n).to_string(),
            ty: *ty,
            nullable: true,
            default: None,
            max_len: None,
            numeric: None,
        })
        .collect()
}

/// Every `nc_catalog` table: name and columns.
pub fn definitions() -> Vec<(String, Vec<Column>)> {
    DEFS.iter()
        .map(|(n, cols)| ((*n).to_string(), columns_of(cols)))
        .collect()
}

/// The `nc_catalog` table `name`.
pub fn lookup(name: &str) -> Option<Table> {
    let (n, cols) = DEFS.iter().find(|(n, _)| *n == name)?;
    Some(Table {
        id: 0,
        name: format!("nc_catalog.{n}"),
        columns: columns_of(cols),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
        next_index_id: 1,
        foreign_keys: Vec::new(),
        referenced_by: Vec::new(),
    })
}

/// A column's type as nucleus writes it.
pub fn type_text(c: &Column) -> String {
    match (c.ty, c.max_len, c.numeric) {
        (DataType::Text, Some(n), _) => format!("varchar({n})"),
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
pub fn rows(name: &str, txn: &dyn KvTxn) -> Result<Vec<Row>> {
    let tables = catalog::tables(txn)?;
    let mut out: Vec<Row> = Vec::new();
    match name {
        "schemas" => {
            for (n, d) in [
                ("nc_catalog", "the nucleus system catalog"),
                ("public", "user tables"),
                ("pg_catalog", "PostgreSQL-compatible catalog views"),
                ("information_schema", "SQL-standard catalog views"),
            ] {
                out.push(vec![text(n), text(d)]);
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
                    ]);
                }
            }
        }
        "constraints" => {
            for c in crate::pgcatalog::constraints(&tables) {
                let kind = match c.kind {
                    "p" => "primary key",
                    "u" => "unique",
                    "c" => "check",
                    _ => "foreign key",
                };
                out.push(vec![
                    id(&c.table),
                    text(c.table.name.clone()),
                    text(c.name),
                    text(kind),
                    text(c.def),
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
        "types" => {
            for (n, aliases, cat) in [
                ("boolean", &["bool"][..], "boolean"),
                (
                    "bigint",
                    &["int", "integer", "smallint", "int8", "int4", "int2"][..],
                    "number",
                ),
                (
                    "double precision",
                    &["float", "real", "float8", "float4"][..],
                    "number",
                ),
                ("numeric", &["decimal", "dec"][..], "number"),
                (
                    "text",
                    &["varchar", "character varying", "char"][..],
                    "text",
                ),
                ("date", &[][..], "datetime"),
                (
                    "timestamp",
                    &["timestamp without time zone"][..],
                    "datetime",
                ),
                ("timestamptz", &["timestamp with time zone"][..], "datetime"),
                ("interval", &[][..], "interval"),
            ] {
                out.push(vec![
                    text(n),
                    Value::Array(aliases.iter().map(|a| text(*a)).collect()),
                    text(cat),
                ]);
            }
        }
        _ => return Err(Error::internal(format!("no nc_catalog table {name}"))),
    }
    Ok(out)
}
