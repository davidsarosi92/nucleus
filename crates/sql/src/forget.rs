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

use nucleus_kv::KvTxn;

use crate::ast::{Expr, SelectItem, SqlType, Statement, TypeMod};
use crate::catalog::{self, Table};
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::{DataType, Value};

const RECORDS: [u8; 2] = [0x00, 0x19];
const HEAD: [u8; 2] = [0x00, 0x1A];
const RECORD_VERSION: u8 = 1;

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
        w.u64(self.rows);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != RECORD_VERSION {
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
        Ok(Self {
            seq,
            at,
            session_user,
            user,
            tables,
            rows,
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
    let value = subject_text(txn, reg, subject, params)?;
    let who = crate::access::current();
    let system = std::sync::Arc::new(crate::access::Who::system());
    let mut tables = Vec::new();
    let mut total = 0;
    for t in ordered(&*txn)? {
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
    let e = Erasure {
        seq,
        at: crate::datetime::clock().unwrap_or_default(),
        session_user: who.session_user.clone(),
        user: who.user.clone(),
        tables,
        rows: total,
    };
    txn.put(&record_key(seq), &e.encode())?;
    txn.put(&HEAD, &seq.to_be_bytes())?;
    Ok(e)
}

/// The columns `FORGET SUBJECT` returns.
pub const RESULT: [&str; 2] = ["erasure", "rows"];
