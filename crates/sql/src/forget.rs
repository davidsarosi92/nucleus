//! Erasure (PLAN.md §M14.24 R2).  `ALTER TABLE t SET SUBJECT (column, …)`
//! marks the columns that say whose a row is (a memory store's `subject`
//! is marked when it is made); `FORGET SUBJECT value` deletes, in one
//! transaction, every row any of them names in every such table — as the
//! caller (its privileges and row-level security policies), with the
//! foreign keys' actions and derived rows following as for any delete —
//! and keeps an *erasure record* of it: when, who, the tables and how many
//! rows, never a value of the subject (`nc_catalog.erasures`).
//!
//! All of a subject or nothing: a row of the subject left behind that row
//! security hid from the caller refuses the statement (`42501`), so an
//! erasure that went through erased every row there was.  Tables are
//! deleted from referencing ones first, so a `RESTRICT` key between two
//! subject tables does not stop it; a key from a table without subject
//! columns acts as it is declared (a `RESTRICT` refuses, naming it).

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::ast::{Expr, SelectItem, SqlType, Statement, TypeMod};
use crate::catalog::{self, Table};
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::{DataType, Value};

const RECORDS: [u8; 2] = [0x00, 0x19];
const HEAD: [u8; 2] = [0x00, 0x1A];
/// Version 2 adds whether the scrub ran and what the search found.
const RECORD_VERSION: u8 = 2;

/// An erasure record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Erasure {
    /// Its number: 1, 2, …
    pub seq: u64,
    /// When (µs since 2000-01-01 UTC).
    pub at: i64,
    /// Who logged in.
    pub session_user: String,
    /// Who acted.
    pub user: String,
    /// The tables rows were deleted from, with how many (the rows the
    /// foreign keys' actions deleted are not counted).
    pub tables: Vec<(String, u64)>,
    /// The rows deleted, in all.
    pub rows: u64,
    /// Whether the scrub after the commit ran through.
    pub scrubbed: bool,
    /// The places in the files the search then found a value of the
    /// subject's rows in (`None`: not searched).
    pub found: Option<u64>,
    /// The audit log's records whose detail it blanked.
    pub audit: u64,
}

fn record_key(seq: u64) -> Vec<u8> {
    let mut k = RECORDS.to_vec();
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

impl Erasure {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(RECORD_VERSION)
            .u64(self.seq)
            .u64(self.at as u64)
            .str(&self.session_user)
            .str(&self.user)
            .u32(self.tables.len() as u32);
        for (t, n) in &self.tables {
            w.str(t).u64(*n);
        }
        w.u64(self.rows).u8(u8::from(self.scrubbed));
        match self.found {
            None => w.u8(0),
            Some(n) => w.u8(1).u64(n),
        };
        w.u64(self.audit);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=RECORD_VERSION).contains(&version) {
            return Err(Error::internal("an erasure record of an unknown version"));
        }
        let seq = r.u64()?;
        let at = r.u64()? as i64;
        let session_user = r.str()?;
        let user = r.str()?;
        let n = r.u32()? as usize;
        let mut tables = Vec::with_capacity(n);
        for _ in 0..n {
            tables.push((r.str()?, r.u64()?));
        }
        let rows = r.u64()?;
        let (scrubbed, found, audit) = if version >= 2 {
            (
                r.u8()? == 1,
                if r.u8()? == 1 { Some(r.u64()?) } else { None },
                r.u64()?,
            )
        } else {
            (false, None, 0)
        };
        Ok(Self {
            seq,
            at,
            session_user,
            user,
            tables,
            rows,
            scrubbed,
            found,
            audit,
        })
    }
}

/// Every erasure record, in order.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Erasure>> {
    let end = nucleus_kv::prefix_end(&RECORDS).expect("erasure prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&RECORDS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        let (_, v) = e?;
        out.push(Erasure::decode(&v)?);
    }
    Ok(out)
}

/// `SET SUBJECT (columns)` or, with none, `DROP SUBJECT`, in `txn`.
pub(crate) fn set(txn: &mut dyn KvTxn, table: &str, columns: &[String]) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    if t.partition_of.is_some() {
        return Err(Error::feature(
            "subject columns are set on the partitioned table, not on a partition",
        ));
    }
    let mut subject = Vec::with_capacity(columns.len());
    for name in columns {
        let c = t.column(name).ok_or_else(|| {
            Error::undefined_column(format!(
                "column \"{name}\" of relation \"{table}\" does not exist"
            ))
        })?;
        if subject.contains(&c) {
            return Err(Error::duplicate_column(format!(
                "column \"{name}\" named twice"
            )));
        }
        subject.push(c);
    }
    t.subject = subject;
    catalog::update_table(txn, &t)
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Runs `stmt`; its rows, or the count its command tag ends with.
fn run(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    stmt: &Statement,
    params: &[Value],
) -> Result<(Vec<Vec<Value>>, u64)> {
    let mut all = crate::session::Collect::default();
    let out = crate::session::execute_statement(txn, reg, stmt, params, &mut all)?;
    Ok(match out {
        crate::session::Output::Rows { rows, .. } => (rows, 0),
        crate::session::Output::Command(tag) => {
            let n = tag
                .rsplit(' ')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            (all.rows, n)
        }
    })
}

fn sql(text: &str) -> Result<Statement> {
    Ok(crate::parser::parse(text)?.remove(0))
}

/// The subject as text: the statement's value cast.
fn subject_text(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    subject: &Expr,
    params: &[Value],
) -> Result<Value> {
    let q = crate::ast::Query {
        with: None,
        body: crate::ast::SetExpr::Select(Box::new(crate::ast::Select {
            distinct: false,
            items: vec![SelectItem::Expr {
                expr: Expr::Cast {
                    expr: Box::new(subject.clone()),
                    ty: SqlType::builtin(DataType::Text),
                    typmod: TypeMod::default(),
                },
                alias: None,
            }],
            from: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            windows: vec![],
        })),
        order_by: vec![],
        limit: None,
        offset: None,
    };
    let (mut rows, _) = run(txn, reg, &Statement::Query(q), params)?;
    let v = rows.pop().and_then(|mut r| r.pop()).unwrap_or(Value::Null);
    if v == Value::Null {
        return Err(Error::not_null_violation(
            "FORGET SUBJECT needs a value, not NULL",
        ));
    }
    Ok(v)
}

/// The tables with subject columns, referencing ones before those they
/// reference (among them; a cycle in no particular order).
fn ordered(txn: &dyn KvTxn) -> Result<Vec<Table>> {
    let mut left: Vec<Table> = catalog::tables(txn)?
        .into_iter()
        .filter(|t| !t.subject.is_empty() && t.partition_of.is_none())
        .collect();
    let mut out = Vec::with_capacity(left.len());
    while !left.is_empty() {
        // A table no other one left references.
        let i = (0..left.len())
            .find(|&i| {
                !left[i]
                    .referenced_by
                    .iter()
                    .any(|(by, _)| by != &left[i].name && left.iter().any(|t| &t.name == by))
            })
            .unwrap_or(0);
        out.push(left.remove(i));
    }
    Ok(out)
}

/// `FORGET SUBJECT subject`, in `txn`: the subject's rows deleted from
/// every table with subject columns, and the erasure record written.
pub(crate) fn forget(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    subject: &Expr,
    params: &[Value],
) -> Result<Erasure> {
    PENDING.with(|p| p.borrow_mut().take());
    let value = subject_text(txn, reg, subject, params)?;
    let who = crate::access::current();
    let system = std::sync::Arc::new(crate::access::Who::system());
    let mut tables = Vec::new();
    let mut total = 0;
    let mut needles = Vec::new();
    // What the audit log's statements would hold of the subject: its value
    // as a literal, its rows' texts.
    let mut said = std::collections::BTreeSet::new();
    if let Value::Text(s) = &value {
        needles_of(&Value::Text(s.clone()), &mut needles);
        said.insert(format!("'{}'", s.replace('\'', "''")));
    }
    let subject_tables = ordered(&*txn)?;
    for t in &subject_tables {
        // The subject in each column's type; a column it cannot be a value
        // of holds none of its rows.
        let mut terms = Vec::new();
        for &c in &t.subject {
            let column = &t.columns[c];
            let cast = format!("CAST($1 AS {})", crate::ncatalog::type_text(column));
            let check = sql(&format!("SELECT {cast}"))?;
            let fits = crate::access::with_who(&system, || {
                run(txn, reg, &check, std::slice::from_ref(&value))
            });
            if fits.is_ok() {
                terms.push(format!("{} = {cast}", quoted(&column.name)));
            }
        }
        if terms.is_empty() {
            continue;
        }
        let filter = terms.join(" OR ");
        let name = quoted(&t.name);
        // What the search after the scrub looks for: the rows' values —
        // but those the table's other rows hold too (the same text, the
        // same vector), which the search would find where they are.
        let select = sql(&format!("SELECT * FROM {name} WHERE {filter}"))?;
        let (rows, _) = crate::access::with_who(&system, || {
            run(txn, reg, &select, std::slice::from_ref(&value))
        })?;
        let mut mine = std::collections::BTreeSet::new();
        for v in rows.iter().flatten() {
            let mut n = Vec::new();
            needles_of(v, &mut n);
            mine.extend(n);
            if let Value::Text(s) = v
                && s.len() >= MIN_TEXT
            {
                said.insert(literal_head(s));
            }
        }
        if !mine.is_empty() {
            let others = sql(&format!(
                "SELECT * FROM {name} WHERE ({filter}) IS NOT TRUE"
            ))?;
            let mut shared = Shared { needles: &mut mine };
            crate::access::with_who(&system, || {
                crate::session::execute_statement(
                    txn,
                    reg,
                    &others,
                    std::slice::from_ref(&value),
                    &mut shared,
                )
            })?;
        }
        needles.extend(mine);
        let (_, deleted) = run(
            txn,
            reg,
            &sql(&format!("DELETE FROM {name} WHERE {filter}"))?,
            std::slice::from_ref(&value),
        )?;
        // All of it or nothing: what row security hid is still there.
        let count = sql(&format!("SELECT count(*) FROM {name} WHERE {filter}"))?;
        let (left, _) = crate::access::with_who(&system, || {
            run(txn, reg, &count, std::slice::from_ref(&value))
        })?;
        if left
            .first()
            .and_then(|r| r.first())
            .is_some_and(|v| *v != Value::Int(0))
        {
            return Err(Error::insufficient_privilege(format!(
                "rows of this subject in \"{}\" are hidden from you by row security: \
                 FORGET SUBJECT erases all of a subject or nothing",
                t.name
            )));
        }
        if deleted > 0 {
            tables.push((t.name.clone(), deleted));
            total += deleted;
        }
    }
    let seq = match txn.get(&HEAD)? {
        Some(h) if h.len() == 8 => u64::from_be_bytes(h[..8].try_into().expect("8")) + 1,
        _ => 1,
    };
    let said: Vec<String> = said.into_iter().collect();
    let mut e = Erasure {
        seq,
        at: crate::datetime::clock().unwrap_or_default(),
        session_user: who.session_user.clone(),
        user: who.user.clone(),
        tables,
        rows: total,
        scrubbed: false,
        found: None,
        audit: 0,
    };
    e.audit = crate::audit::erase(txn, seq, &said)?;
    txn.put(&record_key(seq), &e.encode())?;
    txn.put(&HEAD, &seq.to_be_bytes())?;
    // The scrub, after the commit: every table a delete may have reached
    // — the subject tables, those referencing them (their keys' actions),
    // and their partitions.
    let all = catalog::tables(&*txn)?;
    let mut reached: Vec<Table> = subject_tables;
    let mut i = 0;
    while i < reached.len() {
        let more: Vec<Table> = all
            .iter()
            .filter(|t| {
                !reached.iter().any(|r| r.name == t.name)
                    && (reached[i].referenced_by.iter().any(|(by, _)| *by == t.name)
                        || t.partition_of
                            .as_ref()
                            .is_some_and(|(p, _)| *p == reached[i].name))
            })
            .cloned()
            .collect();
        reached.extend(more);
        i += 1;
    }
    needles.sort();
    needles.dedup();
    PENDING.with(|p| {
        *p.borrow_mut() = Some(Pending {
            seq,
            tables: reached,
            needles,
            audit: e.audit > 0,
            said,
        })
    });
    Ok(e)
}

/// The columns `FORGET SUBJECT` returns: the erasure record's number, the
/// rows deleted, whether the scrub ran through, and what the search after
/// it found.
pub const RESULT: [(&str, DataType); 4] = [
    ("erasure", DataType::BigInt),
    ("rows", DataType::BigInt),
    ("scrubbed", DataType::Bool),
    ("found", DataType::BigInt),
];

/// What a `FORGET`'s transaction leaves for [`finish`], after its commit.
struct Pending {
    seq: u64,
    tables: Vec<Table>,
    needles: Vec<Vec<u8>>,
    /// Whether audit records were blanked (their old versions purged too).
    audit: bool,
    /// What a statement's text holds of the subject.
    said: Vec<String>,
}

thread_local! {
    static PENDING: std::cell::RefCell<Option<Pending>> = const { std::cell::RefCell::new(None) };
}

/// Takes out of `needles` what a row's values give too.
struct Shared<'a> {
    needles: &'a mut std::collections::BTreeSet<Vec<u8>>,
}

impl crate::session::Sink for Shared<'_> {
    fn columns(&mut self, _: &[crate::session::OutColumn]) -> Result<()> {
        Ok(())
    }

    fn row(&mut self, row: crate::types::Row) -> Result<std::ops::ControlFlow<()>> {
        let mut n = Vec::new();
        for v in row.iter() {
            needles_of(v, &mut n);
        }
        for x in &n {
            self.needles.remove(x);
        }
        Ok(if self.needles.is_empty() {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        })
    }
}

/// A text as a statement would hold it: its literal's escaping, its
/// first 64 bytes (a long value's head stands for it).
fn literal_head(s: &str) -> String {
    let lit = s.replace('\'', "''");
    let mut end = lit.len().min(64);
    while !lit.is_char_boundary(end) {
        end -= 1;
    }
    lit[..end].to_string()
}

/// The fewest distinct byte values a needle has.
const MIN_DISTINCT: usize = 8;

/// The shortest text searched for: a shorter one is found by chance.
const MIN_TEXT: usize = 8;

/// The bytes of `v` the search looks for: a text or `bytea` whole (from
/// [`MIN_TEXT`] bytes; a long one by windows of 32 bytes every 128, so a
/// part kept apart — a chunk — is found too), a vector whole, as stored
/// and normalized (an HNSW index's cosine form), another extension type's
/// bytes, an array's items.  Bytes of fewer than [`MIN_DISTINCT`]
/// distinct values — zeros, padding, a sparse vector — say too little:
/// found by chance where others' bytes meet, they are not looked for.
fn needles_of(v: &Value, out: &mut Vec<Vec<u8>>) {
    let mut keep = |b: &[u8]| {
        let mut seen = [false; 256];
        let distinct = b
            .iter()
            .filter(|&&x| !std::mem::replace(&mut seen[x as usize], true))
            .count();
        if distinct >= MIN_DISTINCT {
            out.push(b.to_vec());
        }
    };
    let bytes = |b: &[u8], keep: &mut dyn FnMut(&[u8])| {
        if b.len() < MIN_TEXT {
            return;
        }
        if b.len() <= 64 {
            keep(b);
            return;
        }
        for at in (0..b.len() - 32).step_by(128).take(64) {
            keep(&b[at..at + 32]);
        }
    };
    match v {
        Value::Text(s) | Value::Char(s) => bytes(s.as_bytes(), &mut keep),
        Value::Bytea(b) => bytes(b, &mut keep),
        Value::Ext(t, b) if t.name() == "vector" => {
            keep(b);
            let norm = nucleus_kernels::dot(b, b).sqrt();
            if norm > 0.0 && norm.is_finite() {
                let mut n = Vec::with_capacity(b.len());
                for c in b.as_chunks::<4>().0 {
                    let x = (f64::from(f32::from_le_bytes(*c)) / norm) as f32;
                    n.extend_from_slice(&x.to_le_bytes());
                }
                keep(&n);
            }
        }
        Value::Ext(_, b) => bytes(b, &mut keep),
        Value::Array(items) => {
            for x in items {
                needles_of(x, out);
            }
        }
        _ => {}
    }
}

/// How long the scrub waits for the snapshots older than the erasure.
const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// After a `FORGET`'s commit (PLAN.md §M14.24 R2b): the index methods'
/// structures rewritten without the deleted rows, statistics' samples
/// taken again, then the KV store's scrub ([`Kv::scrub`]: the reached
/// tables' versions purged past history, free pages and the log's dropped
/// segments zeroed) and the search for the rows' values in every file
/// ([`Kv::find_bytes`]); the erasure record updated, `out` (the
/// statement's row) given the scrub's result.
pub(crate) fn finish(
    kv: &std::sync::Arc<dyn Kv>,
    reg: &Registry,
    out: crate::session::Output,
) -> Result<crate::session::Output> {
    let Some(p) = PENDING.with(|p| p.borrow_mut().take()) else {
        return Ok(out);
    };
    let system = std::sync::Arc::new(crate::access::Who::system());
    crate::access::with_who(&system, || scrub_tables(kv, reg, &p.tables))?;
    // What memory keeps by a hash of contents — decoded index objects, a
    // model's vectors of texts — cannot be told by row: all of it goes.
    crate::object_cache::clear();
    crate::models::clear_cache();
    // Statement statistics keep each statement's last text for the index
    // advisor (memory only; shown normalized, without constants).
    crate::statements::erase(kv, &p.said);
    let mut ranges = Vec::new();
    for t in &p.tables {
        let id = t.id.to_be_bytes();
        for prefix in [
            t.data_prefix(),
            [&[0x00, 0x04][..], &id].concat(),
            [&[0x00, 0x17][..], &id].concat(),
            [&[0x00, 0x18][..], &id].concat(),
        ] {
            let end = nucleus_kv::prefix_end(&prefix).expect("a table's prefix");
            ranges.push((prefix, end));
        }
    }
    if p.audit {
        ranges.push(crate::audit::records_range());
    }
    let done = kv.scrub(&ranges, WAIT)?;
    let scrubbed = done.is_some_and(|d| d.complete);
    let found = if scrubbed {
        kv.find_bytes(&p.needles)?
    } else {
        None
    };
    // The record says so.
    let key = record_key(p.seq);
    nucleus_kv::transact(kv.as_ref(), TxnOptions::default(), |txn| {
        if let Some(b) = txn.get(&key)? {
            let mut e = Erasure::decode(&b).map_err(|e| nucleus_kv::Error::Storage(Box::new(e)))?;
            e.scrubbed = scrubbed;
            e.found = found;
            txn.put(&key, &e.encode())?;
        }
        Ok(())
    })?;
    Ok(match out {
        crate::session::Output::Rows { columns, mut rows } => {
            for r in &mut rows {
                if r.len() == RESULT.len() {
                    r[2] = Value::Bool(scrubbed);
                    r[3] = found.map_or(Value::Null, |n| Value::Int(n as i64));
                }
            }
            crate::session::Output::Rows { columns, rows }
        }
        other => other,
    })
}

/// Each reached table's index methods' structures rewritten without the
/// rows gone ([`IndexMethod::scrub`](crate::index_method::IndexMethod::scrub)),
/// and its statistics' sample taken again where it has one.
fn scrub_tables(kv: &std::sync::Arc<dyn Kv>, reg: &Registry, tables: &[Table]) -> Result<()> {
    for t in tables {
        for (i, ix) in t.indexes.iter().enumerate() {
            if let Some(m) = crate::index_method::of(ix)? {
                m.scrub(kv, reg, t, i)?;
            }
        }
        // A column store's segments keep a deleted row's values.
        crate::convert::attempt(kv, |txn| match catalog::table(&*txn, &t.name) {
            Ok(now) if now.id == t.id => crate::column_store::scrub(txn, &now),
            _ => Ok(0),
        })?;
        let analyzed = {
            let txn = kv.begin(TxnOptions::read_only())?;
            txn.get(&catalog::stats_key(t))?.is_some()
        };
        if analyzed {
            let mut txn = kv.begin(TxnOptions::default())?;
            crate::stats::analyze(txn.as_mut(), Some(&t.name))?;
            txn.commit()?;
        }
    }
    Ok(())
}
