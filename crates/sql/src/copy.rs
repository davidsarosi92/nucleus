//! `COPY t FROM STDIN` (PLAN.md §M14.11): rows loaded from the client in
//! one statement, without a statement per row — parsed as the data
//! arrives (PostgreSQL's text format or CSV), or typed rows the protocol
//! has already decoded (`ncp`'s batches).  Each row is checked and stored as
//! an `INSERT`'s is: defaults for the columns not given, constraints,
//! indexes, foreign keys (checked a chunk at a time), row-level security.

use nucleus_kv::KvTxn;

use crate::ast::CopyOptions;
use crate::catalog;
use crate::dml::{RowBuilder, Writer, insert_routed};
use crate::error::{Error, Result};
use crate::expr::Ctx;
use crate::ext::Registry;
use crate::privileges::{INSERT, Kind};
use crate::session::{OutColumn, Sink};
use crate::types::Row;

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

/// The data cut into complete lines or records as it arrives — a scan
/// for their ends, cheap, so the records can be parsed in parallel.
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

/// Complete records: their bytes, and each one's place and line.
#[derive(Default)]
struct Batch {
    data: Vec<u8>,
    records: Vec<(usize, usize, u64)>,
}

impl Batch {
    fn raw(&self, i: usize) -> (&[u8], u64) {
        let (a, b, line) = self.records[i];
        (&self.data[a..b], line)
    }
}

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

    /// Appends `bytes` and adds the records they complete to `batch`
    /// (`last`: the data has ended, so what is left is the last record).
    fn split(&mut self, bytes: &[u8], last: bool, batch: &mut Batch) {
        if self.ended {
            return;
        }
        self.pending.extend_from_slice(bytes);
        let base = batch.data.len();
        let mut start = 0;
        let mut taken = 0;
        while !self.ended {
            let end = match self.record_end(start) {
                Some(end) => end,
                None if last && start < self.pending.len() => self.pending.len(),
                None => break,
            };
            self.line += 1;
            let raw = &self.pending[start..end];
            let raw_end = start + raw.strip_suffix(b"\r").unwrap_or(raw).len();
            if self.header_left {
                self.header_left = false;
            } else if !self.options.csv && &self.pending[start..raw_end] == b"\\." {
                self.ended = true;
            } else {
                batch
                    .records
                    .push((base + start, base + raw_end, self.line));
            }
            start = (end + 1).min(self.pending.len());
            taken = start;
        }
        if self.ended {
            taken = self.pending.len();
        }
        batch.data.extend_from_slice(&self.pending[..taken]);
        self.pending.drain(..taken);
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
}

/// A record's fields, borrowed from the parser.
pub(crate) struct Record<'a> {
    text: &'a str,
    spans: &'a [Option<(u32, u32)>],
}

impl Record<'_> {
    pub(crate) fn len(&self) -> usize {
        self.spans.len()
    }

    /// Field `i`: `None` for NULL.
    pub(crate) fn get(&self, i: usize) -> Option<&str> {
        self.spans[i].map(|(a, b)| &self.text[a as usize..b as usize])
    }
}

/// A record's fields parsed, unescaped, one after another — one buffer for
/// every record a parser reads, not a string per field.
#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    /// Each field's place in `buf` (`None` for NULL).
    spans: Vec<Option<(u32, u32)>>,
}

impl Parser {
    fn parse<'a>(&'a mut self, o: &CopyOptions, raw: &[u8], line: u64) -> Result<Record<'a>> {
        self.buf.clear();
        self.spans.clear();
        if o.csv {
            csv_fields(o, raw, &mut self.buf, &mut self.spans).map_err(|e| at_line(e, line))?;
        } else {
            text_fields(o, raw, &mut self.buf, &mut self.spans);
        }
        let text = std::str::from_utf8(&self.buf).map_err(|_| {
            at_line(
                Error::invalid_text("invalid byte sequence for encoding \"UTF8\""),
                line,
            )
        })?;
        Ok(Record {
            text,
            spans: &self.spans,
        })
    }
}

/// The end of a field just written to `buf` from `from`.
fn span(buf: &[u8], from: usize) -> Option<(u32, u32)> {
    Some((from as u32, buf.len() as u32))
}

/// PostgreSQL's text format: fields split by the delimiter, the NULL
/// string as NULL, backslash escapes.
fn text_fields(
    o: &CopyOptions,
    raw: &[u8],
    buf: &mut Vec<u8>,
    spans: &mut Vec<Option<(u32, u32)>>,
) {
    for f in raw.split(|&c| c == o.delimiter) {
        if f == o.null.as_bytes() {
            spans.push(None);
            continue;
        }
        let from = buf.len();
        if !f.contains(&b'\\') {
            buf.extend_from_slice(f);
            spans.push(span(buf, from));
            continue;
        }
        let mut i = 0;
        while i < f.len() {
            if f[i] != b'\\' || i + 1 == f.len() {
                buf.push(f[i]);
                i += 1;
                continue;
            }
            i += 1;
            let c = f[i];
            i += 1;
            match c {
                b'b' => buf.push(8),
                b'f' => buf.push(12),
                b'n' => buf.push(b'\n'),
                b'r' => buf.push(b'\r'),
                b't' => buf.push(b'\t'),
                b'v' => buf.push(11),
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
                        buf.push(b'x');
                    } else {
                        buf.push(v as u8);
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
                    buf.push(v as u8);
                }
                other => buf.push(other),
            }
        }
        spans.push(span(buf, from));
    }
}

/// CSV: fields split by the delimiter, quoted ones taken as they are
/// (the quote doubled — or escaped — inside), an unquoted field equal to
/// the NULL string NULL.
fn csv_fields(
    o: &CopyOptions,
    raw: &[u8],
    buf: &mut Vec<u8>,
    spans: &mut Vec<Option<(u32, u32)>>,
) -> Result<()> {
    let mut i = 0;
    loop {
        let from = buf.len();
        let mut quoted = false;
        let mut was_quoted = false;
        while i < raw.len() {
            let c = raw[i];
            if quoted {
                // The escape before a quote (or, when it is not the
                // quote, before itself) stands for that character.
                let next = raw.get(i + 1).copied();
                if c == o.escape
                    && (next == Some(o.quote) || (o.escape != o.quote && next == Some(o.escape)))
                {
                    buf.push(next.unwrap_or(c));
                    i += 2;
                    continue;
                }
                if c == o.quote {
                    quoted = false;
                    i += 1;
                    continue;
                }
                buf.push(c);
            } else if c == o.quote {
                quoted = true;
                was_quoted = true;
            } else if c == o.delimiter {
                break;
            } else {
                buf.push(c);
            }
            i += 1;
        }
        if quoted {
            return Err(Error::bad_copy_format("unterminated CSV quoted field"));
        }
        if !was_quoted && buf[from..] == *o.null.as_bytes() {
            buf.truncate(from);
            spans.push(None);
        } else {
            spans.push(span(buf, from));
        }
        if i >= raw.len() {
            return Ok(());
        }
        i += 1; // the delimiter
    }
}

fn at_line(mut e: Error, line: u64) -> Error {
    e.message = format!("{} (COPY data, line {line})", e.message);
    e
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
    let targets = crate::dml::target_columns(&t, columns)?;
    let builder = {
        let ro: &dyn KvTxn = &*txn;
        let binder = crate::binder::Binder::new(ro, reg).checked();
        if let Some(c) = binder.checker() {
            for &col in &targets {
                c.require(ro, Kind::Table, &t.name, INSERT, Some(&t.columns[col].name))?;
            }
        }
        let check = crate::dml::new_row_check(&binder, &t, crate::policies::Command::Insert)?;
        RowBuilder::new(Writer::new(ro, reg, t.clone())?, targets, check)
    };
    let targets = builder.targets();
    sink.copy_in(
        &targets
            .iter()
            .map(|&c| OutColumn {
                name: t.columns[c].name.clone(),
                ty: t.columns[c].ty,
            })
            .collect::<Vec<_>>(),
    )?;
    let arity = |n: usize| -> Result<()> {
        if n == targets.len() {
            return Ok(());
        }
        Err(Error::bad_copy_format(if n < targets.len() {
            format!("missing data for column \"{}\"", t.columns[targets[n]].name)
        } else {
            "extra data after last expected column".to_string()
        }))
    };
    let mut fields = Fields::new(options.clone());
    let workers = crate::budget::workers();
    let parallel = workers > 1 && builder.parallel_safe() && txn.shared().is_some();
    let mut count = 0u64;
    let mut batch = Batch::default();
    // Built rows (with their lines, for errors), and the stored rows whose
    // foreign keys are checked next.
    let mut built: Vec<(Row, u64, Option<Vec<u8>>)> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    loop {
        let chunk = sink.copy_data()?;
        let last = chunk.is_none();
        // A record that fails to build fails the statement after the rows
        // before it are stored (their errors first, as row by row).
        let mut fed = Ok(());
        match chunk {
            Some(Chunk::Rows(typed)) => {
                let ctx = Ctx::new(&*txn, reg, &[]);
                fed = typed.into_iter().try_for_each(|r| {
                    arity(r.len())?;
                    built.push((builder.build_values(&ctx, r)?, 0, None));
                    Ok(())
                });
            }
            Some(Chunk::Bytes(b)) => fields.split(&b, false, &mut batch),
            None => fields.split(&[], true, &mut batch),
        }
        if batch.data.len() >= LOAD_BATCH || (last && !batch.records.is_empty()) {
            let b = std::mem::take(&mut batch);
            let ro: &dyn KvTxn = &*txn;
            let ctx = Ctx::new(ro, reg, &[]);
            // The records `range` of the batch, parsed and built.
            let part = |ctx: &Ctx,
                        range: std::ops::Range<usize>|
             -> Result<Vec<(Row, u64, Option<Vec<u8>>)>> {
                let mut parser = Parser::default();
                let mut out = Vec::with_capacity(range.len());
                for i in range {
                    let (raw, line) = b.raw(i);
                    let row = parser.parse(options, raw, line).and_then(|rec| {
                        arity(rec.len())
                            .and_then(|()| builder.build_text(ctx, &rec))
                            .map_err(|e| at_line(e, line))
                    })?;
                    let encoded = builder.writer.encode_new(&row);
                    out.push((row, line, encoded));
                }
                Ok(out)
            };
            let n = b.records.len();
            let shared = ro.shared().filter(|_| parallel && n >= 2 * PART_RECORDS);
            if let Some(shared) = shared {
                let parts = n.div_ceil(PART_RECORDS);
                let got = crate::vector::on_workers(&ctx, shared, workers, parts, true, &|w, m| {
                    part(w, m * PART_RECORDS..((m + 1) * PART_RECORDS).min(n))
                });
                for r in got {
                    match r {
                        Some(Ok(mut rs)) => built.append(&mut rs),
                        Some(Err(e)) => {
                            fed = Err(e);
                            break;
                        }
                        None => break,
                    }
                }
            } else {
                match part(&ctx, 0..n) {
                    Ok(mut rs) => built.append(&mut rs),
                    Err(e) => fed = Err(e),
                }
            }
        }
        for (row, line, encoded) in built.drain(..) {
            insert_routed(txn, &builder.writer, &row, encoded)
                .map_err(|e| if line > 0 { at_line(e, line) } else { e })?;
            rows.push(row);
            if rows.len() >= STEP {
                crate::fk::check_children(
                    &*txn,
                    &builder.writer.table,
                    rows.iter().map(|r| (None, r)),
                )?;
                count += rows.len() as u64;
                rows.clear();
            }
        }
        fed?;
        if last {
            crate::fk::check_children(
                &*txn,
                &builder.writer.table,
                rows.iter().map(|r| (None, r)),
            )?;
            count += rows.len() as u64;
            return Ok(count);
        }
    }
}

/// Bytes of complete records parsed together (in parallel when the
/// statement has workers).
const LOAD_BATCH: usize = 4 << 20;

/// Records a worker parses at a turn.
const PART_RECORDS: usize = 2048;

#[cfg(test)]
mod tests {
    use super::*;

    fn all(options: CopyOptions, chunks: &[&[u8]]) -> Vec<Vec<Option<String>>> {
        let mut f = Fields::new(options.clone());
        let mut batch = Batch::default();
        for c in chunks {
            f.split(c, false, &mut batch);
        }
        f.split(&[], true, &mut batch);
        let mut p = Parser::default();
        (0..batch.records.len())
            .map(|i| {
                let (raw, line) = batch.raw(i);
                let r = p.parse(&options, raw, line).unwrap();
                (0..r.len()).map(|i| r.get(i).map(str::to_string)).collect()
            })
            .collect()
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
