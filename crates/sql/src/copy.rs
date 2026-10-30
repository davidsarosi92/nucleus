//! `COPY t FROM STDIN` (PLAN.md §M14.11): rows loaded from the client in
//! one statement, without a statement per row — parsed as the data
//! arrives (PostgreSQL's text format or CSV), or typed rows the protocol
//! has already decoded (`ncp`'s batches).  Each row is checked and stored as
//! an `INSERT`'s is: defaults for the columns not given, constraints,
//! indexes, foreign keys (checked a chunk at a time), row-level security.

use std::collections::HashSet;

use nucleus_kv::KvTxn;

use crate::ast::CopyOptions;
use crate::catalog::{self, Table};
use crate::dml::{Writer, assign, insert_routed};
use crate::error::{Error, Result};
use crate::expr::Ctx;
use crate::ext::Registry;
use crate::privileges::{INSERT, Kind};
use crate::session::{OutColumn, Sink};
use crate::types::{Row, Value};

/// A piece of `COPY`'s data, as the front end received it.
#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    /// Bytes of the text or CSV the statement names.
    Bytes(Vec<u8>),
    /// Rows of the columns announced, typed (the protocol decoded them).
    Rows(Vec<Row>),
}

/// Rows whose foreign keys are checked together.
const STEP: usize = 4096;

/// The fields of the next complete line or record, as they arrive.
struct Fields {
    options: CopyOptions,
    /// Bytes not yet a complete record.
    pending: Vec<u8>,
    /// The data's line (for errors).
    line: u64,
    /// The text format's end marker `\.` was seen.
    ended: bool,
    header_left: bool,
}

/// A field: `None` for NULL.
type Record = Vec<Option<String>>;

impl Fields {
    fn new(options: CopyOptions) -> Self {
        let header_left = options.header;
        Fields {
            options,
            pending: Vec::new(),
            line: 0,
            ended: false,
            header_left,
        }
    }

    /// Appends `bytes`; every record they complete (`last`: the data has
    /// ended, so what is left is the last record).
    fn feed(&mut self, bytes: &[u8], last: bool) -> Result<Vec<Record>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            if self.ended {
                self.pending.clear();
                return Ok(out);
            }
            let Some(end) = self.record_end(start) else {
                break;
            };
            let raw = self.pending[start..end].to_vec();
            start = end + 1;
            self.record(&raw, &mut out)?;
        }
        self.pending.drain(..start);
        if last && !self.pending.is_empty() {
            let raw = std::mem::take(&mut self.pending);
            self.record(&raw, &mut out)?;
        }
        Ok(out)
    }

    /// Where the record starting at `start` ends (its newline), if it is
    /// complete: CSV's newlines inside quotes belong to the field.
    fn record_end(&self, start: usize) -> Option<usize> {
        let b = &self.pending[start..];
        if !self.options.csv {
            return b.iter().position(|&c| c == b'\n').map(|i| start + i);
        }
        let (q, e) = (self.options.quote, self.options.escape);
        let mut quoted = false;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if quoted {
                let next = b.get(i + 1).copied();
                if c == e && (next == Some(q) || (e != q && next == Some(e))) {
                    i += 2;
                    continue;
                }
                if c == q {
                    quoted = false;
                }
            } else if c == q {
                quoted = true;
            } else if c == b'\n' {
                return Some(start + i);
            }
            i += 1;
        }
        None
    }

    fn record(&mut self, raw: &[u8], out: &mut Vec<Record>) -> Result<()> {
        self.line += 1;
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if self.header_left {
            self.header_left = false;
            return Ok(());
        }
        if !self.options.csv && raw == b"\\." {
            self.ended = true;
            return Ok(());
        }
        let line = self.line;
        let fields = if self.options.csv {
            self.csv_fields(raw)
        } else {
            self.text_fields(raw)
        }
        .map_err(|e| at_line(e, line))?;
        out.push(fields);
        Ok(())
    }

    /// PostgreSQL's text format: fields split by the delimiter, the NULL
    /// string as NULL, backslash escapes.
    fn text_fields(&self, raw: &[u8]) -> Result<Record> {
        let mut out = Vec::new();
        for f in raw.split(|&c| c == self.options.delimiter) {
            if f == self.options.null.as_bytes() {
                out.push(None);
                continue;
            }
            let mut s = Vec::with_capacity(f.len());
            let mut i = 0;
            while i < f.len() {
                if f[i] != b'\\' || i + 1 == f.len() {
                    s.push(f[i]);
                    i += 1;
                    continue;
                }
                i += 1;
                let c = f[i];
                i += 1;
                match c {
                    b'b' => s.push(8),
                    b'f' => s.push(12),
                    b'n' => s.push(b'\n'),
                    b'r' => s.push(b'\r'),
                    b't' => s.push(b'\t'),
                    b'v' => s.push(11),
                    b'x' => {
                        let hex = |b: u8| (b as char).to_digit(16);
                        let mut v = 0u32;
                        let mut n = 0;
                        while n < 2 && i < f.len() && hex(f[i]).is_some() {
                            v = v * 16 + hex(f[i]).unwrap_or(0);
                            i += 1;
                            n += 1;
                        }
                        if n == 0 {
                            s.push(b'x');
                        } else {
                            s.push(v as u8);
                        }
                    }
                    b'0'..=b'7' => {
                        let mut v = u32::from(c - b'0');
                        let mut n = 1;
                        while n < 3 && i < f.len() && (b'0'..=b'7').contains(&f[i]) {
                            v = v * 8 + u32::from(f[i] - b'0');
                            i += 1;
                            n += 1;
                        }
                        s.push(v as u8);
                    }
                    other => s.push(other),
                }
            }
            out.push(Some(text(s)?));
        }
        Ok(out)
    }

    /// CSV: fields split by the delimiter, quoted ones taken as they are
    /// (the quote doubled — or escaped — inside), an unquoted field equal to
    /// the NULL string NULL.
    fn csv_fields(&self, raw: &[u8]) -> Result<Record> {
        let o = &self.options;
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let mut s = Vec::new();
            let mut quoted = false;
            let mut was_quoted = false;
            while i < raw.len() {
                let c = raw[i];
                if quoted {
                    // The escape before a quote (or, when it is not the
                    // quote, before itself) stands for that character.
                    let next = raw.get(i + 1).copied();
                    if c == o.escape
                        && (next == Some(o.quote)
                            || (o.escape != o.quote && next == Some(o.escape)))
                    {
                        s.push(next.unwrap_or(c));
                        i += 2;
                        continue;
                    }
                    if c == o.quote {
                        quoted = false;
                        i += 1;
                        continue;
                    }
                    s.push(c);
                } else if c == o.quote {
                    quoted = true;
                    was_quoted = true;
                } else if c == o.delimiter {
                    break;
                } else {
                    s.push(c);
                }
                i += 1;
            }
            if quoted {
                return Err(Error::bad_copy_format("unterminated CSV quoted field"));
            }
            if !was_quoted && s == o.null.as_bytes() {
                out.push(None);
            } else {
                out.push(Some(text(s)?));
            }
            if i >= raw.len() {
                return Ok(out);
            }
            i += 1; // the delimiter
        }
    }
}

fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map_err(|_| Error::invalid_text("invalid byte sequence for encoding \"UTF8\""))
}

fn at_line(mut e: Error, line: u64) -> Error {
    e.message = format!("{} (COPY data, line {line})", e.message);
    e
}

/// The columns `columns` names of `t` (all, without a list).
fn targets(t: &Table, columns: &Option<Vec<String>>) -> Result<Vec<usize>> {
    let Some(names) = columns else {
        return Ok((0..t.columns.len()).collect());
    };
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
        .collect()
}

/// Runs `COPY table [(columns)] FROM STDIN`: announces the columns to
/// `sink`, then loads what it hands over; the rows stored.
pub fn copy_from(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    columns: &Option<Vec<String>>,
    options: &CopyOptions,
    sink: &mut dyn Sink,
) -> Result<u64> {
    let t = catalog::table(&*txn, table)?;
    let targets = targets(&t, columns)?;
    let (writer, check) = {
        let ro: &dyn KvTxn = &*txn;
        let binder = crate::binder::Binder::new(ro, reg).checked();
        if let Some(c) = binder.checker() {
            for &col in &targets {
                c.require(ro, Kind::Table, &t.name, INSERT, Some(&t.columns[col].name))?;
            }
        }
        let check = crate::dml::new_row_check(&binder, &t, crate::policies::Command::Insert)?;
        (Writer::new(ro, reg, t.clone())?, check)
    };
    sink.copy_in(
        &targets
            .iter()
            .map(|&c| OutColumn {
                name: t.columns[c].name.clone(),
                ty: t.columns[c].ty,
            })
            .collect::<Vec<_>>(),
    )?;
    let mut fields = Fields::new(options.clone());
    let mut count = 0u64;
    let mut rows: Vec<Row> = Vec::new();
    // A row of the given values, the rest defaults; checked and stored.
    let store = |txn: &mut dyn KvTxn, given: Vec<Value>, rows: &mut Vec<Row>| -> Result<()> {
        if given.len() != targets.len() {
            return Err(Error::bad_copy_format(if given.len() < targets.len() {
                format!(
                    "missing data for column \"{}\"",
                    t.columns[targets[given.len()]].name
                )
            } else {
                "extra data after last expected column".to_string()
            }));
        }
        let row = {
            let ro: &dyn KvTxn = &*txn;
            let ctx = Ctx::new(ro, reg, &[]);
            let mut row: Row = vec![Value::Null; t.columns.len()];
            let mut set = vec![false; t.columns.len()];
            for (&c, v) in targets.iter().zip(given) {
                row[c] = assign(ro, v, &t, c)?;
                set[c] = true;
            }
            for c in 0..t.columns.len() {
                if !set[c] {
                    row[c] = writer.default_of(c, &ctx)?;
                }
            }
            writer.validate(&row, &ctx)?;
            crate::dml::admit(&check, &t, &row, &ctx)?;
            row
        };
        insert_routed(txn, &writer, &row)?;
        rows.push(row);
        Ok(())
    };
    loop {
        let chunk = sink.copy_data()?;
        let last = chunk.is_none();
        match chunk {
            Some(Chunk::Rows(typed)) => {
                for r in typed {
                    store(txn, r, &mut rows)?;
                }
            }
            Some(Chunk::Bytes(b)) => {
                for rec in fields.feed(&b, false)? {
                    let line = fields.line;
                    let given = rec
                        .into_iter()
                        .map(|f| f.map_or(Value::Null, Value::Text))
                        .collect();
                    store(txn, given, &mut rows).map_err(|e| at_line(e, line))?;
                }
            }
            None => {
                for rec in fields.feed(&[], true)? {
                    let line = fields.line;
                    let given = rec
                        .into_iter()
                        .map(|f| f.map_or(Value::Null, Value::Text))
                        .collect();
                    store(txn, given, &mut rows).map_err(|e| at_line(e, line))?;
                }
            }
        }
        if rows.len() >= STEP || (last && !rows.is_empty()) {
            crate::fk::check_children(&*txn, &writer.table, rows.iter().map(|r| (None, r)))?;
            count += rows.len() as u64;
            rows.clear();
        }
        if last {
            return Ok(count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(options: CopyOptions, chunks: &[&[u8]]) -> Vec<Record> {
        let mut f = Fields::new(options);
        let mut out = Vec::new();
        for c in chunks {
            out.extend(f.feed(c, false).unwrap());
        }
        out.extend(f.feed(&[], true).unwrap());
        out
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn text_format_across_chunks() {
        let got = all(
            CopyOptions::default(),
            &[
                b"1\ta\\tb\n2\t\\N",
                b"\n3\t\\x41\\101\\\\\r\n\\.\nignored\n",
            ],
        );
        assert_eq!(
            got,
            vec![
                vec![s("1"), s("a\tb")],
                vec![s("2"), None],
                vec![s("3"), s("AA\\")],
            ]
        );
    }

    #[test]
    fn csv_quotes_newlines_and_nulls() {
        let o = CopyOptions {
            csv: true,
            header: true,
            delimiter: b',',
            null: String::new(),
            ..CopyOptions::default()
        };
        let got = all(o, &[b"id,name\n1,\"a,\"\"b\"\"\n", b"c\"\n2,\n3,\"\"\n4,x"]);
        assert_eq!(
            got,
            vec![
                vec![s("1"), s("a,\"b\"\nc")],
                vec![s("2"), None],
                vec![s("3"), s("")],
                vec![s("4"), s("x")],
            ]
        );
    }
}
