//! `COPY t FROM STDIN` (PLAN.md §M14.11): rows loaded from the client in
//! one statement, without a statement per row — parsed as the data
//! arrives (PostgreSQL's text format or CSV), or typed rows the protocol
//! has already decoded (`ncp`'s batches).  Each row is checked and stored as
//! an `INSERT`'s is: defaults for the columns not given, constraints,
//! indexes, foreign keys (checked a chunk at a time), row-level security.

use nucleus_kv::KvTxn;

use crate::ast::CopyOptions;
use crate::catalog::{self, Table};
use crate::dml::{Built, RowBuilder, Writer, insert_batch};
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
    let load = Load {
        builder: &builder,
        options,
        targets,
        table: &t,
    };
    let workers = crate::budget::workers();
    if workers > 1 && builder.parallel_safe() && txn.shared().is_some() {
        return load.pipelined(txn, reg, sink, workers);
    }
    // One thread: read, parse, store, in turn.
    let mut fields = Fields::new(options.clone());
    let mut batch = Batch::default();
    let mut count = 0u64;
    loop {
        let chunk = sink.copy_data()?;
        let last = chunk.is_none();
        let mut built = Vec::new();
        let fed = {
            let ctx = Ctx::new(&*txn, reg, &[]);
            load.feed(&ctx, None, 1, &mut fields, &mut batch, chunk, &mut built)
        };
        count += load.store(txn, built, fed.is_ok())?;
        fed?;
        if last {
            return Ok(count);
        }
    }
}

/// A row built from a record — its line (0: typed rows, which have none)
/// and its storage's form made beforehand, if any.
type Parsed = (Row, u64, Option<Vec<u8>>);

/// What parsing a load's records and storing its rows share.
struct Load<'a> {
    builder: &'a RowBuilder,
    options: &'a CopyOptions,
    targets: &'a [usize],
    table: &'a Table,
}

impl Load<'_> {
    fn arity(&self, n: usize) -> Result<()> {
        if n == self.targets.len() {
            return Ok(());
        }
        Err(Error::bad_copy_format(if n < self.targets.len() {
            format!(
                "missing data for column \"{}\"",
                self.table.columns[self.targets[n]].name
            )
        } else {
            "extra data after last expected column".to_string()
        }))
    }

    /// Takes in a chunk of the data (`None`: its end): typed rows built,
    /// bytes cut into records, and the records parsed and built once they
    /// make a batch (at the end, whatever is left) — on `workers` over
    /// `shared` when there are many.  The rows go to `out` up to the first
    /// that fails, whose error is returned: it fails the statement after
    /// the rows before it are stored (their errors first, as row by row).
    #[allow(clippy::too_many_arguments)]
    fn feed(
        &self,
        ctx: &Ctx,
        shared: Option<&dyn nucleus_kv::KvRead>,
        workers: usize,
        fields: &mut Fields,
        batch: &mut Batch,
        chunk: Option<Chunk>,
        out: &mut Vec<Parsed>,
    ) -> Result<()> {
        let last = chunk.is_none();
        match chunk {
            Some(Chunk::Rows(typed)) => {
                for r in typed {
                    self.arity(r.len())?;
                    out.push((self.builder.build_values(ctx, r)?, 0, None));
                }
                return Ok(());
            }
            Some(Chunk::Bytes(b)) => fields.split(&b, false, batch),
            None => fields.split(&[], true, batch),
        }
        if !(batch.data.len() >= LOAD_BATCH || (last && !batch.records.is_empty())) {
            return Ok(());
        }
        let b = std::mem::take(batch);
        let n = b.records.len();
        match shared.filter(|_| workers > 1 && n >= 2 * PART_RECORDS) {
            Some(shared) => {
                let parts = n.div_ceil(PART_RECORDS);
                let got = crate::vector::on_workers(ctx, shared, workers, parts, false, &|w, m| {
                    let mut rows = Vec::new();
                    let r = self.records(
                        w,
                        &b,
                        m * PART_RECORDS..((m + 1) * PART_RECORDS).min(n),
                        &mut rows,
                    );
                    Ok((rows, r))
                });
                for r in got {
                    let (mut rows, r) = r.expect("every part runs")?;
                    out.append(&mut rows);
                    r?;
                }
                Ok(())
            }
            None => self.records(ctx, &b, 0..n, out),
        }
    }

    /// The records `range` of `b` parsed and built into `out`, up to the
    /// first that fails.
    fn records(
        &self,
        ctx: &Ctx,
        b: &Batch,
        range: std::ops::Range<usize>,
        out: &mut Vec<Parsed>,
    ) -> Result<()> {
        let mut parser = Parser::default();
        out.reserve(range.len());
        for i in range {
            let (raw, line) = b.raw(i);
            let row = parser.parse(self.options, raw, line).and_then(|rec| {
                self.arity(rec.len())
                    .and_then(|()| self.builder.build_text(ctx, &rec))
                    .map_err(|e| at_line(e, line))
            })?;
            let encoded = self.builder.writer.encode_new(&row);
            out.push((row, line, encoded));
        }
        Ok(())
    }

    /// Stores built rows as one batch ([`insert_batch`]; `fks`: their
    /// foreign keys checked); how many.
    fn store(&self, txn: &mut dyn KvTxn, built: Vec<Parsed>, fks: bool) -> Result<u64> {
        if built.is_empty() {
            return Ok(0);
        }
        let mut lines = Vec::with_capacity(built.len());
        let rows: Vec<Built> = built
            .into_iter()
            .enumerate()
            .map(|(place, (row, line, encoded))| {
                lines.push(line);
                Built {
                    row,
                    encoded,
                    place,
                }
            })
            .collect();
        let count = rows.len() as u64;
        insert_batch(
            txn,
            &self.builder.writer,
            rows,
            fks,
            &|place, e| match lines[place] {
                0 => e,
                line => at_line(e, line),
            },
        )?;
        Ok(count)
    }

    /// The load with parsing beside storing (PLAN.md §M14.20 W3b): a
    /// thread of its own cuts and parses the data — its batches on the
    /// statement's workers — while the statement's thread reads the data
    /// and stores what is parsed.  The transaction is locked between them
    /// ([`nucleus_kv::Locked`]): the parse's reads (an enum's labels, the
    /// catalog) wait out a store, and see the transaction as between two
    /// stores — as a parse between them on one thread would.  At most a
    /// few chunks wait for the parser, and what it parsed from them for the
    /// store.
    fn pipelined(
        &self,
        txn: &mut dyn KvTxn,
        reg: &Registry,
        sink: &mut dyn Sink,
        workers: usize,
    ) -> Result<u64> {
        use std::sync::mpsc::{channel, sync_channel};
        let instance = txn.instance();
        let locked = nucleus_kv::Locked::new(txn);
        let ambient = crate::parallel::Ambient::capture();
        let (to_parse, chunks) = sync_channel::<Option<Chunk>>(4);
        // Unbounded, so the parser never waits to hand over: what it holds
        // is what the few chunks sent since the last look made.
        let (to_store, parsed) = channel::<(Vec<Parsed>, Result<()>, bool)>();
        std::thread::scope(|sc| {
            let (locked, ambient) = (&locked, &ambient);
            let parser = sc.spawn(move || {
                ambient
                    .run(|| {
                        let reads = crate::parallel::SharedTxn(locked, instance);
                        let ctx = Ctx::new(&reads, reg, &[]);
                        let mut fields = Fields::new(self.options.clone());
                        let mut batch = Batch::default();
                        // Until the data ends, a record fails, or the
                        // statement stops listening.
                        while let Ok(chunk) = chunks.recv() {
                            let last = chunk.is_none();
                            let mut out = Vec::new();
                            let fed = self.feed(
                                &ctx,
                                Some(locked),
                                workers,
                                &mut fields,
                                &mut batch,
                                chunk,
                                &mut out,
                            );
                            let stop = last || fed.is_err();
                            if (!out.is_empty() || stop) && to_store.send((out, fed, last)).is_err()
                            {
                                return;
                            }
                            if stop {
                                return;
                            }
                        }
                    })
                    .1
            });
            // What the parser hands over, stored: whether it was the end.
            let mut count = 0u64;
            let mut store = |(rows, fed, last): (Vec<Parsed>, Result<()>, bool)| -> Result<bool> {
                count += self.store(&mut **locked.lock(), rows, fed.is_ok())?;
                fed?;
                Ok(last)
            };
            let stopped = || Error::internal("the COPY parser stopped");
            let done = (|| -> Result<()> {
                let mut reading = true;
                loop {
                    // What is parsed, stored before the next chunk is read.
                    while let Ok(p) = parsed.try_recv() {
                        if store(p)? {
                            return Ok(());
                        }
                    }
                    if reading {
                        let chunk = sink.copy_data()?;
                        reading = chunk.is_some();
                        // A parser gone stopped at a failure: it handed
                        // that over last.
                        if to_parse.send(chunk).is_err() {
                            reading = false;
                        }
                    } else if store(parsed.recv().map_err(|_| stopped())?)? {
                        return Ok(());
                    }
                }
            })();
            drop((to_parse, parsed));
            match parser.join() {
                Ok(scanned) => crate::budget::add_scanned(scanned),
                Err(panic) => std::panic::resume_unwind(panic),
            }
            done.map(|()| count)
        })
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
