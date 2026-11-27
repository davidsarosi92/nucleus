//! Requests on a collection's documents (PLAN.md §M16 Q4, D22): what a
//! driver's `find`, `count`, `insert`, `update`, `replace` and `delete`
//! send as one `ncp` message (`O`), compiled here to one SQL statement
//! over the §M16 Q3 functions — the same plan, transaction and checks as
//! the SQL a user would write.  `ncsql`'s `db.c.find(…)` and the MongoDB
//! wire protocol (§M16 Q5) compile through this too.
//!
//! The filter, the update and the projection go into the statement as
//! literals, so a constant filter is compiled where the statement is bound
//! and a path index serves it (§M16 Q2); the collection's name is quoted
//! as an identifier.

use crate::error::{Error, Result};
use crate::json::document::T;

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The documents a filter matches: rows of one column `doc`.
    Find,
    /// How many: one row, one `bigint`.
    Count,
    /// Insert a document or an array of them: their `_id`s, a row each.
    Insert,
    /// Update by operators: one row — matched, modified, the upserted
    /// document's `_id` (or `NULL`).
    Update,
    /// Replace the document: the same row as an update.
    Replace,
    /// Delete: the count, as `DELETE n`.
    Delete,
}

impl Op {
    /// Its code on the wire.
    pub fn code(self) -> u8 {
        match self {
            Op::Find => 1,
            Op::Count => 2,
            Op::Insert => 3,
            Op::Update => 4,
            Op::Replace => 5,
            Op::Delete => 6,
        }
    }

    /// The op of a code.
    pub fn of(code: u8) -> Result<Op> {
        Ok(match code {
            1 => Op::Find,
            2 => Op::Count,
            3 => Op::Insert,
            4 => Op::Update,
            5 => Op::Replace,
            6 => Op::Delete,
            c => return Err(Error::invalid_parameter(format!("no document request {c}"))),
        })
    }
}

/// A request on a collection.  JSON texts; empty where not given.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// What it does.
    pub op: Option<Op>,
    /// The collection's name (one identifier).
    pub collection: String,
    /// The filter (every document where empty).
    pub filter: String,
    /// `Insert`: a document or an array of them; `Update`: the update
    /// document; `Replace`: the replacement; `Find`: the projection.
    pub body: String,
    /// `Find`: the sort document (its fields in its text's order).
    pub sort: String,
    /// Documents skipped (after the sort).
    pub skip: u64,
    /// At most this many (0: no limit).
    pub limit: u64,
    /// `Update`, `Delete`: every match, not the first.
    pub many: bool,
    /// `Update`, `Replace`: insert when nothing matches.
    pub upsert: bool,
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn parsed(text: &str, what: &str) -> Result<T> {
    let b = crate::json::parse::parse(text)
        .map_err(|e| Error::invalid_parameter(format!("{what}: {}", e.message)))?;
    T::from_bytes(&b)
}

fn operators(body: &T) -> Option<bool> {
    match body {
        T::Obj(o) => o.keys().next().map(|k| k.starts_with('$')),
        _ => None,
    }
}

/// The statement a request is.
pub fn statement(r: &Request) -> Result<String> {
    let op =
        r.op.ok_or_else(|| Error::invalid_parameter("a document request needs an operation"))?;
    if r.collection.is_empty() {
        return Err(Error::invalid_parameter(
            "a document request needs a collection",
        ));
    }
    let c = ident(&r.collection);
    let filter = if r.filter.trim().is_empty() {
        "{}"
    } else {
        r.filter.as_str()
    };
    if !matches!(parsed(filter, "the filter")?, T::Obj(_)) {
        return Err(Error::invalid_parameter("a filter must be a JSON object"));
    }
    // Each part checked whole, before any document (as MongoDB refuses
    // a bad request whether or not anything matches).
    crate::json::path::filter::compile(&crate::json::parse::parse(filter)?)?;
    if !r.sort.trim().is_empty() {
        crate::json::document::sort_fields(&r.sort)?;
    }
    match op {
        Op::Update | Op::Replace if !r.body.trim().is_empty() => {
            crate::json::document::check_update(&crate::json::parse::parse(&r.body)?)?;
        }
        Op::Find if !r.body.trim().is_empty() => {
            crate::json::document::check_projection(&crate::json::parse::parse(&r.body)?)?;
        }
        _ => {}
    }
    let wh = format!("doc @? json_filter({})", lit(filter));
    Ok(match op {
        Op::Find => {
            let what = if r.body.trim().is_empty() {
                "doc".to_string()
            } else {
                format!("json_project(doc, {}) AS doc", lit(&r.body))
            };
            let mut s = format!("SELECT {what} FROM {c} WHERE {wh}");
            if !r.sort.trim().is_empty() {
                s += &format!(" ORDER BY json_sort_key(doc, {})", lit(&r.sort));
            }
            if r.limit > 0 {
                s += &format!(" LIMIT {}", r.limit);
            }
            if r.skip > 0 {
                s += &format!(" OFFSET {}", r.skip);
            }
            s
        }
        Op::Count => {
            if r.limit > 0 || r.skip > 0 {
                let mut inner = format!("SELECT 1 FROM {c} WHERE {wh}");
                if r.limit > 0 {
                    inner += &format!(" LIMIT {}", r.limit);
                }
                if r.skip > 0 {
                    inner += &format!(" OFFSET {}", r.skip);
                }
                format!("SELECT count(*) AS n FROM ({inner}) s")
            } else {
                format!("SELECT count(*) AS n FROM {c} WHERE {wh}")
            }
        }
        Op::Insert => match parsed(&r.body, "the documents")? {
            T::Obj(_) => format!(
                "INSERT INTO {c} (doc) VALUES ({}) RETURNING _id",
                lit(&r.body)
            ),
            T::Arr(a) if !a.is_empty() && a.iter().all(|d| matches!(d, T::Obj(_))) => format!(
                "INSERT INTO {c} (doc) SELECT d FROM json_array_elements({}) WITH ORDINALITY AS e(d, n) ORDER BY n RETURNING _id",
                lit(&r.body)
            ),
            _ => {
                return Err(Error::invalid_parameter(
                    "an insert takes a document or a non-empty array of documents",
                ));
            }
        },
        Op::Update | Op::Replace => {
            let body = parsed(&r.body, "the update")?;
            match (op, operators(&body)) {
                (_, None) => {
                    return Err(Error::invalid_parameter(
                        "an update or a replacement is a non-empty JSON object",
                    ));
                }
                (Op::Update, Some(false)) => {
                    return Err(Error::invalid_parameter(
                        "an update document's keys are operators ($set, …); a document without them is a replacement",
                    ));
                }
                (Op::Replace, Some(true)) => {
                    return Err(Error::invalid_parameter(
                        "a replacement has no operators ($set, …)",
                    ));
                }
                _ => {}
            }
            if op == Op::Replace && r.many {
                return Err(Error::invalid_parameter(
                    "a replacement replaces one document",
                ));
            }
            let u = lit(&r.body);
            let one = if r.many { "" } else { " LIMIT 1" };
            let mut s = format!(
                "WITH m AS (SELECT _id FROM {c} WHERE {wh}{one}), \
                 u AS (UPDATE {c} SET doc = json_update(doc, {u}) \
                 WHERE _id IN (SELECT _id FROM m) AND json_update(doc, {u}) <> doc RETURNING _id)"
            );
            if r.upsert {
                s += &format!(
                    ", i AS (INSERT INTO {c} (doc) SELECT json_upsert_document({}, {u}) \
                     WHERE NOT EXISTS (SELECT 1 FROM m) RETURNING _id)",
                    lit(filter)
                );
            }
            s += " SELECT (SELECT count(*) FROM m) AS matched, (SELECT count(*) FROM u) AS modified, ";
            s += if r.upsert {
                "(SELECT _id FROM i) AS upserted"
            } else {
                "NULL::json AS upserted"
            };
            s
        }
        Op::Delete => {
            if r.many {
                format!("DELETE FROM {c} WHERE {wh}")
            } else {
                format!("DELETE FROM {c} WHERE _id IN (SELECT _id FROM {c} WHERE {wh} LIMIT 1)")
            }
        }
    })
}
