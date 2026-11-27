//! Changefeeds (PLAN.md §M16.5 S3, D23): `CREATE CHANGEFEED f ON t
//! [WITH (before, retention = …, max_entries = …)]` — a stream (§M16.5
//! S2, [`crate::stream`]) that every write of `t` appends to in the
//! writing transaction: one entry a row inserted, updated or deleted,
//! `(seq, time, op, key, after, before)`.  So a feed holds every
//! committed change of its table in commit order and none of a rolled
//! back one, numbered without a gap — the transactional outbox without
//! the outbox, and a change log read from neither the MVCC versions nor
//! the WAL.
//!
//! The table keeps its feeds ([`Table::changefeeds`]: their ids, which
//! give their keys, and whether they want the row before); its writers'
//! storage is wrapped ([`wrap`]) so that each row's write appends to them.
//! No change is missed: a writer reads the table's definition validated
//! at its commit, so a write that began before a feed was made and
//! commits after it fails (`40001`) and is run again with it; and the
//! transaction making the feed validates the table's whole key range, so
//! a write committed after its snapshot (by a writer that saw no feed)
//! fails it instead.  What that snapshot reads of the table and what the
//! feed holds after it are, together, every change.

use std::sync::Arc;

use nucleus_kv::KvTxn;

use crate::ast::{ColumnSpec, SqlType};
use crate::catalog::{self, Changefeed, Table};
use crate::encoding::RowWriter;
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::storage::{BatchSource, NewRow, RowIter, Scan, TableStorage};
use crate::types::{DataType, Row, Value};

/// A feed's columns after `seq` and `time`.
pub const OP: &str = "op";
/// The changed row's key, as a `json` object of its primary key's columns
/// (`{"rowid": n}` for a table without one).
pub const KEY: &str = "key";
/// The row after the change (`NULL` for a delete).
pub const AFTER: &str = "after";
/// The row before it (`NULL` for an insert, and unless the feed was made
/// `WITH (before)`).
pub const BEFORE: &str = "before";

/// Feed `name`, which must be a changefeed (`42809` otherwise), and its
/// table's id.
fn of(txn: &dyn KvTxn, name: &str) -> Result<(Table, u32)> {
    let t = crate::stream::of(txn, name)?;
    match t.stream.as_ref().and_then(|s| s.source) {
        Some(source) => Ok((t, source)),
        None => Err(Error::wrong_object_type(format!(
            "\"{name}\" is not a changefeed"
        ))),
    }
}

/// The table with id `id`, if there is one.
pub(crate) fn table_by_id(txn: &dyn KvTxn, id: u32) -> Result<Option<Table>> {
    Ok(catalog::tables(txn)?.into_iter().find(|t| t.id == id))
}

/// `CREATE CHANGEFEED name ON table [WITH (…)]`, in `txn`.
pub(crate) fn create(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    table: &str,
    with: &[(String, String)],
) -> Result<()> {
    let mut source = catalog::table(&*txn, table)?;
    let refused = |why: &str| {
        Err(Error::feature(format!(
            "a changefeed on \"{table}\", {why}: not yet"
        )))
    };
    if crate::stream::is_stream(&source) {
        return Err(Error::wrong_object_type(format!(
            "\"{table}\" is a stream: its entries are its changes (CONSUME, stream_wait)"
        )));
    }
    if source.partition_by.is_some() {
        return refused("a partitioned table");
    }
    if source.chunks.is_some() {
        return refused("a chunk table");
    }
    if source.partition_of.is_some() {
        return refused("a partition");
    }
    if !source.subject.is_empty() {
        // Its rows' copies would outlive FORGET SUBJECT.
        return refused("a table with subject columns");
    }
    let mut before = false;
    let mut rest = Vec::new();
    for (k, v) in with {
        match k.as_str() {
            "before" => {
                before = match v.as_str() {
                    "true" | "on" => true,
                    "false" | "off" => false,
                    _ => {
                        return Err(Error::invalid_parameter(format!(
                            "a changefeed's before is true or false, not \"{v}\""
                        )));
                    }
                }
            }
            _ => rest.push((k.clone(), v.clone())),
        }
    }
    let json = |name: &str, not_null: bool| ColumnSpec {
        name: name.into(),
        ty: if name == OP {
            SqlType::builtin(DataType::Text)
        } else {
            SqlType::Named {
                schema: None,
                name: "json".into(),
            }
        },
        max_len: None,
        numeric: None,
        not_null,
        primary_key: false,
        unique: false,
        default: None,
        check: None,
        references: None,
        generated: None,
        auto: None,
    };
    let columns = [
        json(OP, true),
        json(KEY, true),
        json(AFTER, false),
        json(BEFORE, false),
    ];
    // A write of the table committed after this transaction's snapshot
    // by a writer that read the table without the feed is in neither:
    // the commit fails on it (all the table's keys, whatever its storage
    // keeps them under).
    let all = source.index_prefix(0)[..5].to_vec();
    let end = nucleus_kv::prefix_end(&all).expect("a table's prefix");
    txn.validate_range(&all, &end)?;
    crate::stream::create(txn, reg, name, &columns, &[], &rest)?;
    let mut feed = catalog::table(&*txn, name)?;
    if let Some(s) = feed.stream.as_mut() {
        s.source = Some(source.id);
    }
    catalog::update_table(txn, &feed)?;
    source.changefeeds.push(Changefeed {
        id: feed.id,
        before,
    });
    catalog::update_table(txn, &source)
}

/// `DROP CHANGEFEED`'s check: each named is a feed.
pub(crate) fn check_drop(txn: &dyn KvTxn, name: &str) -> Result<()> {
    of(txn, name).map(|_| ())
}

/// What dropping table `t` (in `txn`, with the tables `dropped`) does to
/// changefeeds: a feed leaves its table's list; a table with feeds not
/// dropped with it is refused (`2BP01`) — with `cascade`, their names to
/// drop first are returned.
pub(crate) fn on_drop(
    txn: &mut dyn KvTxn,
    t: &Table,
    dropped: &[String],
    cascade: bool,
) -> Result<Vec<String>> {
    if let Some(source) = t.stream.as_ref().and_then(|s| s.source)
        && let Some(mut s) = table_by_id(&*txn, source)?
    {
        s.changefeeds.retain(|f| f.id != t.id);
        catalog::update_table(txn, &s)?;
    }
    let mut first = Vec::new();
    for f in &t.changefeeds {
        let Some(feed) = table_by_id(&*txn, f.id)? else {
            continue;
        };
        if dropped.contains(&feed.name) {
            continue;
        }
        if !cascade {
            return Err(Error::dependent_objects(format!(
                "cannot drop table {} because other objects depend on it: changefeed {}",
                t.name, feed.name
            ))
            .with_hint("DROP CHANGEFEED first, or DROP TABLE … CASCADE"));
        }
        first.push(feed.name);
    }
    Ok(first)
}

/// The name of the table feed `t` follows, if it is a changefeed whose
/// table is there.
pub fn source_of(txn: &dyn KvTxn, t: &Table) -> Result<Option<String>> {
    match t.stream.as_ref().and_then(|s| s.source) {
        None => Ok(None),
        Some(id) => Ok(table_by_id(txn, id)?.map(|s| s.name)),
    }
}

// ------------------------------------------------------------ the writes

/// `storage` of table `t` — wrapped, where `t` has changefeeds, so that
/// each row it writes is appended to them too.
pub(crate) fn wrap(t: &Table, storage: Arc<dyn TableStorage>) -> Arc<dyn TableStorage> {
    if t.changefeeds.is_empty() {
        return storage;
    }
    Arc::new(Feeding { inner: storage })
}

/// A table's storage that appends its changes to the table's feeds.
struct Feeding {
    inner: Arc<dyn TableStorage>,
}

/// A row as a `json` object of `t`'s columns (beyond them: a column being
/// retyped's hidden values, left out).
fn object(t: &Table, row: &[Value]) -> Result<Value> {
    let mut pairs = Vec::with_capacity(t.columns.len());
    for (c, v) in t.columns.iter().zip(row) {
        pairs.push((c.name.as_str(), crate::json::ops::from_value(v)?));
    }
    Ok(crate::json::value(crate::json::ops::object_of(pairs)?))
}

/// A row's key as a `json` object: its primary key's columns, or the
/// hidden row id its key ends with.
fn key(t: &Table, key: &[u8], row: &[Value]) -> Result<Value> {
    if t.primary_key.is_empty() {
        let id = key
            .get(t.index_prefix(0).len()..)
            .ok_or_else(|| Error::internal("a row key shorter than its prefix"))?;
        let (ids, _) = crate::encoding::decode_key(id, 1)?;
        let rowid = crate::json::ops::from_value(&ids[0])?;
        return Ok(crate::json::value(crate::json::ops::object_of(vec![(
            "rowid", rowid,
        )])?));
    }
    let mut pairs = Vec::with_capacity(t.primary_key.len());
    for &c in &t.primary_key {
        pairs.push((
            t.columns[c].name.as_str(),
            crate::json::ops::from_value(&row[c])?,
        ));
    }
    Ok(crate::json::value(crate::json::ops::object_of(pairs)?))
}

/// Appends a change of `t`'s row under `k` to its feeds: `after` the row
/// written (none: deleted), `before` the row it replaced or deleted.
fn changed(
    txn: &mut dyn KvTxn,
    t: &Table,
    k: &[u8],
    after: Option<&Row>,
    before: Option<&Row>,
) -> Result<()> {
    let op = match (after, before) {
        (Some(_), None) => "insert",
        (Some(_), Some(_)) => "update",
        (None, _) => "delete",
    };
    let Some(either) = after.or(before) else {
        return Ok(());
    };
    let key = key(t, k, either)?;
    let after = after.map(|r| object(t, r)).transpose()?;
    let wanted = t.changefeeds.iter().any(|f| f.before);
    let before = match before {
        Some(r) if wanted => Some(object(t, r)?),
        _ => None,
    };
    let entry = |with_before: bool| {
        let mut w = RowWriter::new(6);
        // seq (bigint) and time (timestamptz): the commit's (stream::fill).
        w.raw(&[2, 0, 0, 0, 0, 0, 0, 0, 0]);
        w.raw(&[8, 0, 0, 0, 0, 0, 0, 0, 0]);
        w.value(&Value::Text(op.into()));
        w.value(&key);
        w.value(after.as_ref().unwrap_or(&Value::Null));
        w.value(match (&before, with_before) {
            (Some(b), true) => b,
            _ => &Value::Null,
        });
        w.finish()
    };
    for f in &t.changefeeds {
        let head = catalog::index_prefix_of(f.id, crate::stream::HEAD_INDEX);
        let mut entries = catalog::index_prefix_of(f.id, 0);
        entries.push(crate::stream::BIGINT_TAG);
        txn.append(&head, &entries, &entry(f.before), crate::stream::fill)?;
    }
    Ok(())
}

impl TableStorage for Feeding {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn index_ids(&self, t: &Table) -> Vec<u32> {
        self.inner.index_ids(t)
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        self.inner.write(txn, t, key, row, old)?;
        changed(txn, t, key, Some(row), old)
    }

    fn insert(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<bool> {
        let stored = self.inner.insert(txn, t, key, row)?;
        if stored {
            changed(txn, t, key, Some(row), None)?;
        }
        Ok(stored)
    }

    fn insert_batch(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        rows: Vec<NewRow>,
        new_keys: bool,
    ) -> Result<Option<usize>> {
        let kept: Vec<(Vec<u8>, Row)> = rows
            .iter()
            .map(|r| (r.key.clone(), r.row.clone()))
            .collect();
        let stopped = self.inner.insert_batch(txn, t, rows, new_keys)?;
        let stored = stopped.unwrap_or(kept.len());
        for (k, r) in &kept[..stored] {
            changed(txn, t, k, Some(r), None)?;
        }
        Ok(stopped)
    }

    fn encode_new(&self, t: &Table, row: &Row) -> Option<Vec<u8>> {
        self.inner.encode_new(t, row)
    }

    fn insert_encoded(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        encoded: Vec<u8>,
    ) -> Result<bool> {
        let stored = self.inner.insert_encoded(txn, t, key, row, encoded)?;
        if stored {
            changed(txn, t, key, Some(row), None)?;
        }
        Ok(stored)
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        let mut labels = crate::enums::Labels::default();
        let old = self.inner.get(&*txn, t, key, None, &mut labels)?;
        self.inner.remove(txn, t, key)?;
        changed(txn, t, key, None, old.as_ref())
    }

    fn remove_row(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<()> {
        self.inner.remove_row(txn, t, key, row)?;
        changed(txn, t, key, None, Some(row))
    }

    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        self.inner.exists(txn, t, key, validated)
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut crate::enums::Labels,
    ) -> Result<Option<Row>> {
        self.inner.get(txn, t, key, wanted, labels)
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
        self.inner.get_many(txn, t, keys, wanted, labels, out)
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
        self.inner.rows_at(txn, t, keys, wanted, density, f)
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        self.inner.rows(scan)
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        self.inner.batches(scan)
    }

    fn samples_in_parts(&self) -> bool {
        self.inner.samples_in_parts()
    }

    fn sample(&self, scan: Scan<'_>, sample: &mut crate::stats::Sample) -> Result<()> {
        self.inner.sample(scan, sample)
    }

    fn split_points(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        fine: bool,
    ) -> Result<Vec<Vec<u8>>> {
        self.inner.split_points(txn, t, start, end, parts, fine)
    }

    fn after_writes(&self, txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
        self.inner.after_writes(txn, t)
    }

    fn after_failure(&self, t: &Table) {
        self.inner.after_failure(t)
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        self.inner.check(txn, t, problems)
    }
}
