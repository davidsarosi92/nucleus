//! Collections (PLAN.md §M15 D3): tables of documents.
//!
//! `CREATE COLLECTION c` makes a table `(_id json PRIMARY KEY, doc json)`
//! marked a collection in the catalog ([`Table::collection`]): `_id` is a
//! stored generated column, `doc -> '_id'`, so a read by id is a primary
//! key read and the document round-trips whole.  A document written
//! without an `_id` is given one before its generated columns are
//! computed ([`FILL`], bound with the table's writer): on an update the
//! row's own (a document replaced whole keeps its id), on an insert a
//! UUIDv7 as a string (time-ordered: new documents append at the key
//! range's end).  A document must be an object and its `_id` anything
//! but an array (MongoDB's rule) — two `CHECK`s, `doc_object` and
//! `id_not_array`; `WITH (schema = '…')` adds a third, `doc_schema`,
//! `json_matches_schema(doc, '…')` (§M14.15 J5), set or taken away by
//! `ALTER COLLECTION c SET (schema = '…') | RESET (schema)`.
//!
//! Everything else is the table's: SQL reads and writes it as it is,
//! upserts by `INSERT … ON CONFLICT (_id)` (§M15 D2), indexes,
//! row-level security, time travel, branches — and PostgreSQL's clients
//! see a table.

use nucleus_kv::KvTxn;

use crate::ast::AlterCollection;
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;

/// The id's column: the document's `_id`.
pub const ID: &str = "_id";
/// The document's column.
pub const DOC: &str = "doc";
/// The `CHECK` holding a collection's JSON Schema.
pub const SCHEMA_CHECK: &str = "doc_schema";

/// A document as written: given its row's `_id` where it has none (an
/// update replacing it whole), else a new one (an insert).  Over the
/// table's row; `NULL` stays `NULL` (refused as the key's).
pub const FILL: &str = "CASE WHEN doc ? '_id' THEN doc \
     WHEN _id IS NOT NULL THEN doc || json_build_object('_id', _id) \
     ELSE doc || json_build_object('_id', uuidv7()::text) END";

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn run(txn: &mut dyn KvTxn, reg: &Registry, sql: &str) -> Result<()> {
    let stmt = crate::parser::parse(sql)?.remove(0);
    crate::session::execute_statement(
        txn,
        reg,
        &stmt,
        &[],
        &mut crate::session::Collect::default(),
    )?;
    Ok(())
}

/// The `CHECK` of a collection's documents by `schema` (JSON Schema).
fn schema_check(schema: &str) -> Result<String> {
    // Refused here, not at the first write, if it is no JSON.
    crate::json::parse::parse(schema).map_err(|e| {
        Error::invalid_parameter(format!(
            "a collection's schema is a JSON Schema: {}",
            e.message
        ))
    })?;
    Ok(format!(
        "CONSTRAINT {SCHEMA_CHECK} CHECK (json_matches_schema(doc, {}))",
        literal(schema)
    ))
}

/// `CREATE COLLECTION name [WITH (schema = '…', storage = …, engine =
/// …)]`, in `txn`.
pub(crate) fn create(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    options: &[(String, String)],
) -> Result<()> {
    let mut checks = vec![
        "CONSTRAINT doc_object CHECK (json_typeof(doc) = 'object')".to_string(),
        "CONSTRAINT id_not_array CHECK (json_typeof(_id) <> 'array')".to_string(),
    ];
    let mut with = Vec::new();
    for (k, v) in options {
        match k.as_str() {
            "schema" => checks.push(schema_check(v)?),
            "storage" | "engine" => with.push(format!("{k} = {}", literal(v))),
            _ => {
                return Err(Error::invalid_parameter(format!(
                    "unrecognized collection option \"{k}\" (schema, storage, engine)"
                )));
            }
        }
    }
    let with = if with.is_empty() {
        String::new()
    } else {
        format!(" WITH ({})", with.join(", "))
    };
    run(
        txn,
        reg,
        &format!(
            "CREATE TABLE {} (_id json GENERATED ALWAYS AS (doc -> '_id') STORED PRIMARY KEY, \
             doc json NOT NULL, {}){with}",
            quote(name),
            checks.join(", ")
        ),
    )?;
    let mut table = catalog::table(&*txn, name)?;
    table.collection = true;
    catalog::update_table(txn, &table)
}

/// Table `name`, which must be a collection (`42809` otherwise).
pub(crate) fn of(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    let t = catalog::table(txn, name)?;
    if !t.collection {
        return Err(Error::wrong_object_type(format!(
            "\"{name}\" is not a collection"
        )));
    }
    Ok(t)
}

/// `ALTER COLLECTION name …`, in `txn`.
pub(crate) fn alter(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    action: &AlterCollection,
) -> Result<()> {
    of(&*txn, name)?;
    let t = quote(name);
    match action {
        AlterCollection::Rename(to) => run(
            txn,
            reg,
            &format!("ALTER TABLE {t} RENAME TO {}", quote(to)),
        ),
        AlterCollection::ResetSchema => run(
            txn,
            reg,
            &format!("ALTER TABLE {t} DROP CONSTRAINT IF EXISTS {SCHEMA_CHECK}"),
        ),
        AlterCollection::SetSchema(schema) => {
            let check = schema_check(schema)?;
            run(
                txn,
                reg,
                &format!("ALTER TABLE {t} DROP CONSTRAINT IF EXISTS {SCHEMA_CHECK}"),
            )?;
            run(txn, reg, &format!("ALTER TABLE {t} ADD {check}"))
        }
    }
}

/// A collection's JSON Schema, if it has one.
pub fn schema(t: &Table) -> Option<String> {
    let (_, text) = t.checks.iter().find(|(n, _)| n == SCHEMA_CHECK)?;
    // `json_matches_schema(doc, '…')`: the literal's text.
    let start = text.find('\'')? + 1;
    let end = text.rfind('\'')?;
    (start <= end).then(|| text[start..end].replace("''", "'"))
}
