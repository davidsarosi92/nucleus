//! The extension point for protocol adapters (PLAN.md §M16.5 S6, D21): a
//! module that maps an external protocol onto a stream's appends
//! (ingest) or onto its entries read in order from a `seq` (egress) —
//! as `modules/ndjson` maps newline-delimited JSON over TCP, and as a
//! market-data or a messaging protocol could.
//!
//! - [`append_json`] appends a JSON object (or an array of them) to a
//!   stream in one statement — its fields by the stream's column names,
//!   each converted to its column's type as `JSON_TABLE` converts.
//! - [`Subscription`] reads a stream's entries after a `seq` in order,
//!   each as a JSON object (`seq` and `time` first), waiting for the next
//!   without holding a transaction, and fails naming the gap when entries
//!   it would read were trimmed by the retention: a protocol's resend
//!   request or rewind replays from a number, and learns which numbers it
//!   cannot have.
//!
//! - [`members`] reads a request that came as a JSON object.
//!
//! Both run as statements of the adapter's session (its user's
//! privileges, row policies and audit).

use std::time::Duration;

use crate::error::{Error, Result};
use crate::session::{Output, Session, TxnStatus};
use crate::types::{Row, Value};

/// A member of a JSON object read by [`members`].
#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer that fits 64 bits.
    Int(i64),
    /// A string.
    Text(String),
    /// Anything else (an object, an array, another number): its JSON text.
    Json(String),
}

/// The members of JSON object `text` in its order (a protocol's request);
/// `22P02` if it is not JSON, `22023` if not an object.
pub fn members(text: &str) -> Result<Vec<(String, Member)>> {
    use crate::json::form::{Node, root, text_of};
    let v = crate::json::from_text(text)?;
    let b = crate::json::bytes(&v)?;
    let Node::Object(o) = root(b)? else {
        return Err(Error::invalid_parameter("a request is a JSON object"));
    };
    let mut out = Vec::with_capacity(o.len());
    for i in 0..o.len() {
        let m = match o.value(i)? {
            Node::Null => Member::Null,
            Node::Bool(b) => Member::Bool(b),
            Node::Int(n) => Member::Int(n),
            Node::Str(s) => Member::Text(text_of(s)?.to_string()),
            _ => Member::Json(crate::json::to_text(o.value_bytes(i)?)?),
        };
        out.push((o.key(i)?.to_string(), m));
    }
    Ok(out)
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn rows(s: &mut Session, sql: &str, params: &[Value]) -> Result<(Vec<String>, Vec<Row>)> {
    let stmt = crate::parser::parse(sql)?.remove(0);
    match s.run(&stmt, params)? {
        Output::Rows { columns, rows } => Ok((columns.into_iter().map(|c| c.name).collect(), rows)),
        Output::Command(tag) => Err(Error::internal(format!("{tag}: no rows"))),
    }
}

/// Stream `name`'s columns after `seq` and `time`, each with its type's
/// SQL name; refused (`42809`) for a table that is not a stream.
fn columns(s: &mut Session, name: &str) -> Result<Vec<(String, String)>> {
    let (_, kind) = rows(
        s,
        "SELECT count(*) FROM nc_catalog.streams WHERE name = $1",
        &[Value::Text(name.into())],
    )?;
    if !matches!(kind.first().and_then(|r| r.first()), Some(Value::Int(1))) {
        let (_, any) = rows(
            s,
            "SELECT count(*) FROM nc_catalog.tables WHERE name = $1",
            &[Value::Text(name.into())],
        )?;
        return Err(
            if matches!(any.first().and_then(|r| r.first()), Some(Value::Int(1))) {
                Error::wrong_object_type(format!("\"{name}\" is not a stream"))
            } else {
                Error::undefined_table(format!("relation \"{name}\" does not exist"))
            },
        );
    }
    let (_, cols) = rows(
        s,
        "SELECT name, type FROM nc_catalog.columns WHERE table_name = $1 AND position > 2 \
         ORDER BY position",
        &[Value::Text(name.into())],
    )?;
    Ok(cols
        .into_iter()
        .filter_map(|r| match (r.first(), r.get(1)) {
            (Some(Value::Text(n)), Some(Value::Text(t))) => Some((n.clone(), t.clone())),
            _ => None,
        })
        .collect())
}

/// Appends to stream `name` the JSON object `json`, or each object of the
/// array `json` is, in one statement of `s` (its transaction, or one of
/// its own): fields named as the stream's columns, converted to their
/// types (a value that does not convert refuses the whole append, as
/// `JSON_TABLE`'s `ERROR ON ERROR`); fields it has no column for are passed
/// over, columns no field names are `NULL`.  How many entries were
/// appended.
pub fn append_json(s: &mut Session, name: &str, json: &str) -> Result<u64> {
    let cols = columns(s, name)?;
    if cols.is_empty() {
        return Err(Error::invalid_parameter(format!(
            "stream \"{name}\" has no columns to fill"
        )));
    }
    let names: Vec<String> = cols.iter().map(|(n, _)| quote(n)).collect();
    let defs: Vec<String> = cols
        .iter()
        .map(|(n, ty)| {
            // A name as a JSON path's member: in double quotes.  (A `json`
            // column takes the member as JSON, as `FORMAT JSON` would.)
            let path = format!("$.\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\""));
            format!("{} {ty} PATH {} ERROR ON ERROR", quote(n), literal(&path))
        })
        .collect();
    // One object or many: an array either way, of objects only.
    let array = {
        use crate::json::form::{Node, root};
        let v = crate::json::from_text(json)?;
        let b = crate::json::bytes(&v)?;
        let objects = match root(b)? {
            Node::Object(_) => true,
            Node::Array(a) => (0..a.len()).all(|i| matches!(a.get(i), Ok(Node::Object(_)))),
            _ => false,
        };
        if !objects {
            return Err(Error::invalid_parameter(
                "entries are a JSON object or an array of objects",
            ));
        }
        if json.trim_start().starts_with('[') {
            json.to_string()
        } else {
            format!("[{json}]")
        }
    };
    let sql = format!(
        "INSERT INTO {} ({}) SELECT {} FROM JSON_TABLE($1::json, '$[*]' COLUMNS ({})) entries",
        quote(name),
        names.join(", "),
        names.join(", "),
        defs.join(", ")
    );
    let stmt = crate::parser::parse(&sql)?.remove(0);
    let out = s.run(&stmt, &[Value::Text(array)])?;
    Ok(match out {
        Output::Command(tag) => tag
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0),
        Output::Rows { rows, .. } => rows.len() as u64,
    })
}

/// A stream's entries read in order after a `seq` (D21's egress): each
/// call of [`next`](Self::next) gives the entries after the last one it
/// gave, as JSON objects, waiting for the next when there is none.  Its
/// position is the caller's to keep (a protocol's own sequence number,
/// a client's resend request); a consumer group (`CONSUME`) keeps one in
/// the database instead.
#[derive(Clone, Debug)]
pub struct Subscription {
    stream: String,
    after: i64,
}

impl Subscription {
    /// Stream `stream`'s entries after `after` (0: from the first).
    pub fn new(stream: &str, after: i64) -> Self {
        Subscription {
            stream: stream.to_string(),
            after: after.max(0),
        }
    }

    /// The stream.
    pub fn stream(&self) -> &str {
        &self.stream
    }

    /// The last `seq` given (where the next read starts after).
    pub fn after(&self) -> i64 {
        self.after
    }

    /// The next entries — at most `max`, each a JSON object of the
    /// entry's columns (`seq`, `time`, then the stream's), in `seq` order
    /// with no gap — read by `s` outside a transaction (each read sees
    /// what was committed by then); none yet, it waits up to `timeout`
    /// (holding no transaction) for one, and gives what came (perhaps
    /// none).  Entries it would read that the retention trimmed fail it
    /// (`22000`, naming the `seq`s gone and where the stream now
    /// begins); its position stays, so the caller can choose to go on
    /// with [`skip_to`](Self::skip_to).
    pub fn next(&mut self, s: &mut Session, max: u32, timeout: Duration) -> Result<Vec<String>> {
        if s.status() != TxnStatus::Idle {
            return Err(Error::active_txn(
                "a subscription reads outside a transaction block (each read a snapshot of its own)",
            ));
        }
        let mut entries = self.read(s, max)?;
        if entries.is_empty() && !timeout.is_zero() {
            s.wait_stream(&self.stream, self.after, timeout)?;
            entries = self.read(s, max)?;
        }
        Ok(entries)
    }

    /// The first `seq` the stream keeps now (the newest's next when it
    /// keeps none): where a subscription that met a gap can go on from.
    pub fn start(&self, s: &mut Session) -> Result<i64> {
        let (_, begins) = rows(
            s,
            "SELECT stream_start($1)",
            &[Value::Text(self.stream.clone())],
        )?;
        match begins.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n),
            _ => Err(Error::internal("stream_start gave no number")),
        }
    }

    /// Goes on after `seq` (a caller that accepts a gap).
    pub fn skip_to(&mut self, seq: i64) {
        self.after = seq.max(0);
    }

    fn read(&mut self, s: &mut Session, max: u32) -> Result<Vec<String>> {
        let (names, found) = rows(
            s,
            &format!(
                "SELECT * FROM {} WHERE seq > $1 ORDER BY seq LIMIT $2",
                quote(&self.stream)
            ),
            &[Value::Int(self.after), Value::Int(i64::from(max.max(1)))],
        )?;
        // Where the stream begins now: entries between the position and it
        // were trimmed (whatever row policies show of the rest).
        let start = self.start(s)?;
        if start > self.after + 1 {
            return Err(Error::data_exception(format!(
                "entries {} to {} of stream \"{}\" are gone (its retention trimmed them); it now begins at {start}",
                self.after + 1,
                start - 1,
                self.stream
            )));
        }
        let mut out = Vec::with_capacity(found.len());
        for r in &found {
            let mut pairs = Vec::with_capacity(names.len());
            for (n, v) in names.iter().zip(r) {
                pairs.push((n.as_str(), crate::json::ops::from_value(v)?));
            }
            let doc = crate::json::ops::object_of(pairs)?;
            out.push(crate::json::to_text(&doc)?);
        }
        if let Some(Value::Int(n)) = found.last().and_then(|r| r.first()) {
            self.after = *n;
        }
        Ok(out)
    }
}
