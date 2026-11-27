//! Streams (PLAN.md §M16.5 S2, D23): append-only tables whose key the
//! commit gives.
//!
//! `CREATE STREAM s (columns) [WITH (retention = '7 days', max_entries =
//! n)]` makes a table of storage [`STREAM`] whose first two columns are
//! `seq bigint` (its primary key) and `time timestamptz`, then the
//! columns given.  `INSERT INTO s (…)` appends: the row is held until the
//! commit, which gives it the stream's next `seq` — dense from 1, in
//! commit order (the engine's [`KvTxn::append`]) — and the commit's time,
//! both written into the stored row ([`fill`]); so a snapshot sees a
//! prefix of the stream without a gap, and a consumer that read to `seq`
//! n never later finds an n − 1.  Appenders never conflict.  Everything
//! else reads it as a table: `SELECT … FROM s WHERE seq > $1 ORDER BY
//! seq` is a range of its key.  Entries are not updated or deleted; they
//! are trimmed from the head by the retention ([`trim`], the server's
//! maintenance), and `DROP STREAM` drops them all.
//!
//! A consumer group's position is a key of the stream's
//! ([`CONSUMERS_INDEX`]): `CONSUME FROM s GROUP g [LIMIT n]` returns the
//! entries after it and moves it past them in the same transaction — so
//! a consumer's work and its position commit together (exactly once), and
//! two consumers of one group that took the same entries conflict (one
//! retries).  `ALTER STREAM s SEEK GROUP g TO n` sets it (a replay), `…
//! DROP GROUP g` forgets it.  `stream_wait(s, after, timeout_ms)` waits
//! for an entry past `after` and gives the newest `seq` (its transaction's
//! snapshot does not move: it is for the next one).

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::ast::{AlterStream, ColumnSpec, SqlType, TableConstraint};
use crate::catalog::{self, StreamSpec, Table};
use crate::encoding::RowWriter;
use crate::error::{Error, Result};
use crate::ext::{Arg, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility};
use crate::session::{OutColumn, Output};
use crate::storage::{BatchSource, RowIter, Scan, TableStorage};
use crate::types::{DataType, Row, Value};

/// The storage of a stream's table.
pub const STREAM: &str = "stream";
/// The key column: the entry's number.
pub const SEQ: &str = "seq";
/// The commit's time.
pub const TIME: &str = "time";
/// The index id the stream's head is kept under (the last `seq` given
/// and its time).
pub const HEAD_INDEX: u32 = u32::MAX - 6;
/// The index id consumer groups' positions are kept under.
pub const CONSUMERS_INDEX: u32 = u32::MAX - 7;
/// The tag a `bigint` key value starts with ([`crate::encoding`]).
const BIGINT_TAG: u8 = 0x02;

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Whether `t` is a stream.
pub fn is_stream(t: &Table) -> bool {
    t.storage == STREAM
}

/// Its head's key.
pub fn head_key(t: &Table) -> Vec<u8> {
    t.index_prefix(HEAD_INDEX)
}

/// The prefix its entries' keys begin with.
fn entries_prefix(t: &Table) -> Vec<u8> {
    let mut p = t.index_prefix(0);
    p.push(BIGINT_TAG);
    p
}

/// Group `group`'s position's key.
fn consumer_key(t: &Table, group: &str) -> Vec<u8> {
    let mut k = t.index_prefix(CONSUMERS_INDEX);
    k.extend_from_slice(group.as_bytes());
    k
}

/// Table `name`, which must be a stream (`42809` otherwise).
pub(crate) fn of(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    let t = catalog::table(txn, name)?;
    if !is_stream(&t) {
        return Err(Error::wrong_object_type(format!(
            "\"{name}\" is not a stream"
        )));
    }
    Ok(t)
}

fn refused(what: &str) -> Error {
    Error::feature(format!(
        "{what} a stream's entries: they are appended, and trimmed by its retention"
    ))
}

// ------------------------------------------------------------ the rows

/// A row as appended: `seq` and `time` as fixed 8-byte placeholders the
/// commit fills ([`fill`]), the other columns as stored rows are (inline:
/// an entry's values are its own).
fn encode(t: &Table, row: &Row) -> Vec<u8> {
    let stored = t.to_stored(row);
    let mut w = RowWriter::new(stored.len());
    for (i, v) in stored.iter().enumerate() {
        match i {
            // bigint, 8 bytes (tag 2), and timestamptz (tag 8).
            0 => w.raw(&[2, 0, 0, 0, 0, 0, 0, 0, 0]),
            1 => w.raw(&[8, 0, 0, 0, 0, 0, 0, 0, 0]),
            _ => {
                w.value(v);
            }
        }
    }
    w.finish()
}

/// Writes the entry's number and the commit's time (nanoseconds since
/// the Unix epoch) into an [`encode`]d row: its first two columns.
fn fill(v: &mut [u8], n: u64, nanos: u64) {
    let columns = usize::from(u16::from_le_bytes([v[1], v[2]]));
    let at = 3 + 2 * (columns.saturating_sub(1) / crate::encoding::STRIDE);
    let micros = (nanos / 1000) as i64 - crate::datetime::UNIX_TO_PG_MICROS;
    v[at + 1..at + 9].copy_from_slice(&(n as i64).to_le_bytes());
    v[at + 10..at + 18].copy_from_slice(&micros.to_le_bytes());
}

/// A stream's storage: appends on insert, rows read as the row storage
/// reads them, no update or delete.
pub struct StreamStorage;

fn rows() -> Arc<dyn TableStorage> {
    crate::storage::named(crate::storage::ROW).expect("the row storage is built in")
}

impl TableStorage for StreamStorage {
    fn name(&self) -> &str {
        STREAM
    }

    fn index_ids(&self, _: &Table) -> Vec<u32> {
        vec![0, HEAD_INDEX, CONSUMERS_INDEX]
    }

    fn write(
        &self,
        _: &mut dyn KvTxn,
        _: &Table,
        _: &[u8],
        _: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        if old.is_some() {
            return Err(refused("UPDATE cannot change"));
        }
        Err(Error::internal("a stream's row written by its key"))
    }

    fn insert(&self, txn: &mut dyn KvTxn, t: &Table, _: &[u8], row: &Row) -> Result<bool> {
        txn.append(&head_key(t), &entries_prefix(t), &encode(t, row), fill)?;
        Ok(true)
    }

    fn encode_new(&self, t: &Table, row: &Row) -> Option<Vec<u8>> {
        Some(encode(t, row))
    }

    fn insert_encoded(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        _: &[u8],
        _: &Row,
        encoded: Vec<u8>,
    ) -> Result<bool> {
        txn.append(&head_key(t), &entries_prefix(t), &encoded, fill)?;
        Ok(true)
    }

    fn remove(&self, _: &mut dyn KvTxn, _: &Table, _: &[u8]) -> Result<()> {
        Err(refused("DELETE cannot remove"))
    }

    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        rows().exists(txn, t, key, validated)
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut crate::enums::Labels,
    ) -> Result<Option<Row>> {
        rows().get(txn, t, key, wanted, labels)
    }

    fn get_many(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        keys: &[Vec<u8>],
        wanted: Option<&[bool]>,
        labels: &mut crate::enums::Labels,
        out: &mut Vec<Option<Row>>,
    ) -> Result<()> {
        rows().get_many(txn, t, keys, wanted, labels, out)
    }

    fn rows_at(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        keys: &[Vec<u8>],
        wanted: Option<&[bool]>,
        density: &mut Option<f64>,
        f: &mut dyn FnMut(usize, Row) -> Result<()>,
    ) -> Result<()> {
        rows().rows_at(txn, t, keys, wanted, density, f)
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        rows().rows(scan)
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        rows().batches(scan)
    }

    fn samples_in_parts(&self) -> bool {
        true
    }

    fn sample(&self, scan: Scan<'_>, sample: &mut crate::stats::Sample) -> Result<()> {
        rows().sample(scan, sample)
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        rows().check(txn, t, problems)
    }
}

// ------------------------------------------------------------ statements

/// Refuses what would change a stream as a table: its key and time are
/// the commit's, its entries appended — so no index, no column added,
/// dropped or changed, no other storage, no `TRUNCATE` (its retention
/// trims it); renaming, row security, policies and statistics stay.
pub(crate) fn refuse_change(txn: &dyn KvTxn, stmt: &crate::ast::Statement) -> Result<()> {
    use crate::ast::{Statement as S, TableChange};
    let (tables, what): (Vec<&String>, &str) = match stmt {
        S::CreateIndex { table, .. } => (vec![table], "an index on"),
        S::AlterColumnType { table, .. }
        | S::SetColumnModel { table, .. }
        | S::DropConstraint { table, .. }
        | S::AddColumn { table, .. } => (vec![table], "ALTER TABLE on"),
        S::SetStorage { table, .. }
        | S::SetTableOptions { table, .. }
        | S::SetStoragePolicy { table, .. } => (vec![table], "another storage for"),
        S::SetExpiry { table, .. } => (vec![table], "an expiry on"),
        S::SetSubject { table, .. } => (vec![table], "subject columns on"),
        S::AttachPartition { table, .. } | S::DetachPartition { table, .. } => {
            (vec![table], "partitions of")
        }
        S::AlterTable { table, change } if !matches!(change, TableChange::Rename(_)) => {
            (vec![table], "ALTER TABLE on")
        }
        S::Truncate { tables, .. } => (tables.iter().collect(), "TRUNCATE of"),
        _ => return Ok(()),
    };
    for name in tables {
        if let Some(t) = catalog::get_table(txn, name)?
            && is_stream(&t)
        {
            return Err(Error::feature(format!(
                "{what} a stream (\"{name}\") is not supported: its seq and time are the commit's, its entries appended and trimmed by its retention"
            )));
        }
    }
    Ok(())
}

/// The checks an `INSERT` into stream `t` makes: `seq` and `time` are
/// the commit's (`428C9` for a value given), so `RETURNING` could not
/// show them and `ON CONFLICT` has nothing to conflict with.
pub(crate) fn check_insert(
    t: &Table,
    targets: &[usize],
    returning: bool,
    on_conflict: bool,
) -> Result<()> {
    if let Some(&c) = targets.iter().find(|&&c| c < 2) {
        return Err(Error::generated_always(format!(
            "cannot insert a value into column \"{}\": a stream's seq and time are given by the commit",
            t.columns[c].name
        )));
    }
    if returning {
        return Err(Error::feature(
            "INSERT … RETURNING into a stream: its seq is given at the commit",
        ));
    }
    if on_conflict {
        return Err(Error::feature(
            "INSERT … ON CONFLICT into a stream: its entries never conflict",
        ));
    }
    Ok(())
}

fn options(options: &[(String, String)]) -> Result<(StreamSpec, Vec<(String, String)>)> {
    let mut spec = StreamSpec::default();
    let mut rest = Vec::new();
    for (k, v) in options {
        match k.as_str() {
            "retention" => spec.retention = Some(retention(v)?),
            "max_entries" => {
                spec.max_entries =
                    Some(v.parse::<u64>().ok().filter(|&n| n > 0).ok_or_else(|| {
                        Error::invalid_parameter(format!(
                            "a stream's max_entries is a positive whole number, not \"{v}\""
                        ))
                    })?)
            }
            "engine" => rest.push((k.clone(), v.clone())),
            _ => {
                return Err(Error::invalid_parameter(format!(
                    "unrecognized stream option \"{k}\" (retention, max_entries, engine)"
                )));
            }
        }
    }
    Ok((spec, rest))
}

fn retention(v: &str) -> Result<crate::datetime::Interval> {
    let i = crate::datetime::parse_interval(v).map_err(|_| {
        Error::invalid_parameter(format!(
            "a stream's retention is an interval ('7 days'), not \"{v}\""
        ))
    })?;
    if i.months < 0 || i.days < 0 || i.micros < 0 || (i.months, i.days, i.micros) == (0, 0, 0) {
        return Err(Error::invalid_parameter(
            "a stream's retention is a positive interval",
        ));
    }
    Ok(i)
}

/// `CREATE STREAM name (columns, constraints) [WITH (…)]`, in `txn`.
pub(crate) fn create(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    columns: &[ColumnSpec],
    constraints: &[TableConstraint],
    with: &[(String, String)],
) -> Result<()> {
    let (spec, with) = options(with)?;
    for c in columns {
        if c.name == SEQ || c.name == TIME {
            return Err(Error::duplicate_column(format!(
                "column \"{}\" is a stream's own (seq, time)",
                c.name
            )));
        }
        if c.primary_key || c.unique {
            return Err(Error::feature(
                "a stream's key is seq: its columns are neither PRIMARY KEY nor UNIQUE",
            ));
        }
    }
    if constraints.iter().any(|c| {
        matches!(
            c,
            TableConstraint::PrimaryKey(_) | TableConstraint::Unique(..)
        )
    }) {
        return Err(Error::feature(
            "a stream's key is seq: it has no other PRIMARY KEY or UNIQUE constraint",
        ));
    }
    let builtin = |ty| SqlType::Builtin { schema: None, ty };
    let own = |name: &str, ty: SqlType, primary_key: bool| ColumnSpec {
        name: name.into(),
        ty,
        max_len: None,
        numeric: None,
        not_null: true,
        primary_key,
        unique: false,
        default: Some(if primary_key { "0" } else { "now()" }.into()),
        check: None,
        references: None,
        generated: None,
        auto: None,
    };
    let mut specs = vec![
        own(SEQ, builtin(DataType::BigInt), true),
        own(TIME, builtin(DataType::TimestampTz), false),
    ];
    specs.extend(columns.iter().cloned());
    let mut opts = crate::session::TableOptions::defaults()?.with(&with)?;
    opts.storage = STREAM.into();
    crate::session::create_table(txn, reg, name, &specs, constraints, &opts, None)?;
    let mut t = catalog::table(&*txn, name)?;
    t.stream = Some(spec);
    catalog::update_table(txn, &t)
}

/// `ALTER STREAM name …`, in `txn`.
pub(crate) fn alter(txn: &mut dyn KvTxn, name: &str, action: &AlterStream) -> Result<()> {
    let mut t = of(&*txn, name)?;
    match action {
        AlterStream::Set(opts) => {
            let (spec, rest) = options(opts)?;
            if !rest.is_empty() {
                return Err(Error::feature("a stream's engine is set when it is made"));
            }
            let s = t.stream.get_or_insert_with(StreamSpec::default);
            if spec.retention.is_some() {
                s.retention = spec.retention;
            }
            if spec.max_entries.is_some() {
                s.max_entries = spec.max_entries;
            }
            catalog::update_table(txn, &t)
        }
        AlterStream::Reset(keys) => {
            let s = t.stream.get_or_insert_with(StreamSpec::default);
            for k in keys {
                match k.as_str() {
                    "retention" => s.retention = None,
                    "max_entries" => s.max_entries = None,
                    _ => {
                        return Err(Error::invalid_parameter(format!(
                            "unrecognized stream option \"{k}\" (retention, max_entries)"
                        )));
                    }
                }
            }
            catalog::update_table(txn, &t)
        }
        AlterStream::Seek { group, to } => {
            if *to < 0 {
                return Err(Error::invalid_parameter("a position is 0 or more"));
            }
            txn.put(&consumer_key(&t, group), &to.to_le_bytes())?;
            Ok(())
        }
        AlterStream::DropGroup(group) => {
            txn.delete(&consumer_key(&t, group))?;
            Ok(())
        }
    }
}

/// The columns `CONSUME [wanted] FROM name` returns: those named, in
/// their order (none: all of the stream's), `seq` first whatever is
/// named — the position moves by it.
pub(crate) fn columns(txn: &dyn KvTxn, name: &str, wanted: &[String]) -> Result<Vec<OutColumn>> {
    let t = of(txn, name)?;
    Ok(picked(&t, wanted)?
        .into_iter()
        .map(|i| {
            let c = &t.columns[i];
            OutColumn {
                name: c.name.clone(),
                ty: c.ty,
                not_null: !c.nullable,
            }
        })
        .collect())
}

/// The columns' places `CONSUME` returns ([`columns`]).
fn picked(t: &Table, wanted: &[String]) -> Result<Vec<usize>> {
    if wanted.is_empty() {
        return Ok((0..t.columns.len()).collect());
    }
    let mut out = vec![0];
    for w in wanted {
        let i = t.column(w).ok_or_else(|| {
            Error::undefined_column(format!(
                "column \"{w}\" of stream \"{}\" does not exist",
                t.name
            ))
        })?;
        if !out.contains(&i) {
            out.push(i);
        }
    }
    Ok(out)
}

/// Group `group`'s position in `t`: the last `seq` it consumed (0: none).
fn position(txn: &dyn KvTxn, t: &Table, group: &str, validated: bool) -> Result<i64> {
    let key = consumer_key(t, group);
    let v = if validated {
        txn.get_validated(&key)?
    } else {
        txn.get(&key)?
    };
    Ok(v.and_then(|v| {
        v.get(..8)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
    })
    .unwrap_or(0))
}

/// `CONSUME FROM name GROUP group [LIMIT n]`, in `txn`: the entries after
/// the group's position (as `SELECT * FROM name` sees them: its
/// privileges and row policies), the position moved past them.
pub(crate) fn consume(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    wanted: &[String],
    group: &str,
    limit: Option<i64>,
) -> Result<Output> {
    let t = of(&*txn, name)?;
    let cols = picked(&t, wanted)?;
    if group.is_empty() {
        return Err(Error::invalid_parameter("a consumer group has a name"));
    }
    let limit = limit.unwrap_or(100);
    if limit < 0 {
        return Err(Error::invalid_parameter("LIMIT is 0 or more"));
    }
    // Read validated: another consumer of the group moving it first makes
    // this transaction fail (and the two do not both take the entries).
    let from = position(&*txn, &t, group, true)?;
    let list: Vec<String> = cols.iter().map(|&i| quote(&t.columns[i].name)).collect();
    let sql = format!(
        "SELECT {} FROM {} WHERE {SEQ} > $1 ORDER BY {SEQ} LIMIT $2",
        list.join(", "),
        quote(&t.name)
    );
    let stmt = crate::parser::parse(&sql)?.remove(0);
    let mut sink = crate::session::Collect::default();
    crate::session::execute_statement(
        txn,
        reg,
        &stmt,
        &[Value::Int(from), Value::Int(limit)],
        &mut sink,
    )?;
    let columns = columns(&*txn, name, wanted)?;
    if let Some(Value::Int(last)) = sink.rows.last().and_then(|r| r.first()) {
        txn.put(&consumer_key(&t, group), &last.to_le_bytes())?;
    }
    Ok(Output::Rows {
        columns,
        rows: sink.rows,
    })
}

/// Every consumer group of `t`: its name and position.
pub fn groups(txn: &dyn KvTxn, t: &Table) -> Result<Vec<(String, i64)>> {
    let prefix = t.index_prefix(CONSUMERS_INDEX);
    let mut out = Vec::new();
    for (k, v) in catalog::scan_prefix(txn, &prefix)? {
        let name = String::from_utf8_lossy(&k[prefix.len()..]).into_owned();
        let at = v
            .get(..8)
            .map_or(0, |b| i64::from_le_bytes(b.try_into().unwrap()));
        out.push((name, at));
    }
    Ok(out)
}

/// The last `seq` given in `t` as `txn` sees it, and its time (0 and
/// `None` for none).
pub fn last(txn: &dyn KvTxn, t: &Table) -> Result<(i64, Option<i64>)> {
    Ok(match txn.get(&head_key(t))? {
        Some(v) if v.len() >= 16 => {
            let n = u64::from_be_bytes(v[..8].try_into().unwrap());
            let nanos = u64::from_be_bytes(v[8..16].try_into().unwrap());
            (
                n as i64,
                Some((nanos / 1000) as i64 - crate::datetime::UNIX_TO_PG_MICROS),
            )
        }
        _ => (0, None),
    })
}

// ------------------------------------------------------------ retention

/// Trims every stream's entries its retention no longer keeps — older
/// than `retention` before `now` (µs, PostgreSQL's epoch), or beyond the
/// newest `max_entries` — from the head, in transactions of their own;
/// how many were removed.  The head stays, so numbers are never reused.
pub fn trim_all(kv: &Arc<dyn Kv>, now: i64) -> Result<u64> {
    let tables = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
    };
    let mut removed = 0;
    for t in tables {
        let Some(spec) = &t.stream else { continue };
        if spec.retention.is_none() && spec.max_entries.is_none() {
            continue;
        }
        removed += trim(kv, &t, spec, now)?;
    }
    Ok(removed)
}

/// At most this many entries a trimming transaction removes.
const TRIM_BATCH: usize = 10_000;

fn trim(kv: &Arc<dyn Kv>, t: &Table, spec: &StreamSpec, now: i64) -> Result<u64> {
    let cutoff = spec
        .retention
        .map(|i| {
            crate::datetime::ts_add(
                now,
                crate::datetime::Interval {
                    months: -i.months,
                    days: -i.days,
                    micros: -i.micros,
                },
            )
        })
        .transpose()?;
    let start = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&start).expect("a table's prefix ends");
    let mut removed = 0u64;
    loop {
        let mut attempt = 0;
        let n = loop {
            let mut txn = kv.begin(TxnOptions::default())?;
            let r = (|| -> Result<usize> {
                let (last_seq, _) = last(&*txn, t)?;
                let keep_from = spec
                    .max_entries
                    .map_or(0, |m| last_seq.saturating_sub(m as i64 - 1).max(0));
                let mut gone = Vec::new();
                for item in txn.scan(
                    std::ops::Bound::Included(&start),
                    std::ops::Bound::Excluded(&end),
                )? {
                    let (k, v) = item?;
                    let row = crate::encoding::decode_row(&v, 2)?;
                    let (Some(Value::Int(seq)), Some(Value::TimestampTz(at))) =
                        (row.first(), row.get(1))
                    else {
                        return Err(Error::internal("a stream's entry without seq and time"));
                    };
                    let old = cutoff.is_some_and(|c| *at < c);
                    if !(old || *seq < keep_from) || gone.len() == TRIM_BATCH {
                        break;
                    }
                    gone.push(k);
                }
                for k in &gone {
                    txn.delete(k)?;
                }
                Ok(gone.len())
            })();
            let r = r.and_then(|n| {
                txn.commit()?;
                Ok(n)
            });
            match r {
                Ok(n) => break n,
                Err(e) if e.code == "40001" && attempt < 20 => {
                    attempt += 1;
                    kv.back_off(attempt);
                }
                Err(e) => return Err(e),
            }
        };
        removed += n as u64;
        if n < TRIM_BATCH {
            break;
        }
    }
    if removed > 0 {
        // The head's keys deleted, as a queue's: scans need not step over them.
        kv.vacuum_range(&start, &end)?;
    }
    Ok(removed)
}

// ------------------------------------------------------------ functions

/// `stream_wait(stream, after, timeout_ms)`: waits until an entry past
/// `after` is visible or `timeout_ms` have passed, and gives the stream's
/// newest `seq`.
struct Wait;

/// `stream_position(stream, group)`: the group's position.
struct Position;

/// Registers `stream_wait` and `stream_position`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["stream_wait"], Arc::new(Wait))?;
    r.add_function(&["stream_position"], Arc::new(Position))?;
    Ok(())
}

fn readable(cx: &CallContext<'_>, name: &str) -> Result<Table> {
    let t = of(cx.txn(), name)?;
    let c = crate::privileges::Checker::new();
    if !c.has(
        cx.txn(),
        crate::privileges::Kind::Table,
        &t.name,
        crate::privileges::SELECT,
        None,
    )? {
        return Err(crate::privileges::denied(
            crate::privileges::Kind::Table,
            &t.name,
            crate::privileges::SELECT,
            None,
        ));
    }
    Ok(t)
}

/// The longest wait.
const MAX_WAIT: Duration = Duration::from_secs(3600);

impl ScalarFunction for Wait {
    fn name(&self) -> &str {
        "stream_wait"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        3..=3
    }

    fn resolve(&self, _: &[crate::ext::ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![
                Arg::Untyped(DataType::Text),
                Arg::Untyped(DataType::BigInt),
                Arg::Untyped(DataType::BigInt),
            ],
            Returns::Type(Some(DataType::BigInt)),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Volatile
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (Value::Text(name), Value::Int(after), Value::Int(ms)) = (&args[0], &args[1], &args[2])
        else {
            return Ok(Value::Null);
        };
        let t = readable(cx, name)?;
        let wait = Duration::from_millis((*ms).max(0) as u64).min(MAX_WAIT);
        let after = (*after).max(0) as u64;
        let n = cx.txn().wait_appended(&head_key(&t), after, wait)?;
        Ok(Value::Int(n as i64))
    }
}

impl ScalarFunction for Position {
    fn name(&self) -> &str {
        "stream_position"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }

    fn resolve(&self, _: &[crate::ext::ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Text), Arg::Untyped(DataType::Text)],
            Returns::Type(Some(DataType::BigInt)),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (Value::Text(name), Value::Text(group)) = (&args[0], &args[1]) else {
            return Ok(Value::Null);
        };
        let t = readable(cx, name)?;
        Ok(Value::Int(position(cx.txn(), &t, group, false)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_writes_the_first_two_columns() {
        for n in [3usize, 17, 40] {
            let mut columns = vec![Value::Int(0), Value::TimestampTz(0)];
            columns.extend((2..n).map(|i| Value::Text(format!("c{i}"))));
            let mut w = RowWriter::new(n);
            w.raw(&[2, 0, 0, 0, 0, 0, 0, 0, 0]);
            w.raw(&[8, 0, 0, 0, 0, 0, 0, 0, 0]);
            for v in &columns[2..] {
                w.value(v);
            }
            let mut v = w.finish();
            let nanos = 1_700_000_000_123_456_789u64;
            fill(&mut v, 42, nanos);
            let row = crate::encoding::decode_row(&v, n).unwrap();
            assert_eq!(row[0], Value::Int(42));
            assert_eq!(
                row[1],
                Value::TimestampTz(1_700_000_000_123_456 - crate::datetime::UNIX_TO_PG_MICROS)
            );
            assert_eq!(row[2..], columns[2..]);
        }
    }
}
