//! Row expiry (PLAN.md §M14.24 R1): `ALTER TABLE t SET EXPIRY (column)`
//! — rows whose value in `column` (a `timestamptz`, `timestamp` or
//! `date`) has passed are deleted in the background by the maintenance
//! thread ([`expire`]), as ordinary deletes (their foreign keys' actions,
//! indexes and derived rows follow).  A `NULL` never expires.  `ALTER
//! TABLE t DROP EXPIRY` stops it.  Queries still see an expired row until
//! it is deleted; a memory store's `recall` leaves them out itself.

use std::sync::Arc;

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::catalog;
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::{DataType, Value};

/// Whether `ty` can say when a row expires.
pub fn is_time(ty: DataType) -> bool {
    matches!(
        ty,
        DataType::TimestampTz | DataType::Timestamp | DataType::Date
    )
}

/// `SET EXPIRY (column)` or, with none, `DROP EXPIRY`, in `txn`.
pub(crate) fn set(txn: &mut dyn KvTxn, table: &str, column: Option<&str>) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    t.expiry = match column {
        None => None,
        Some(name) => {
            let c = t.column(name).ok_or_else(|| {
                Error::undefined_column(format!(
                    "column \"{name}\" of relation \"{table}\" does not exist"
                ))
            })?;
            if !is_time(t.columns[c].ty) {
                return Err(Error::type_mismatch(format!(
                    "column \"{name}\" is of type {}: an expiry is a timestamptz, timestamp or date",
                    crate::ncatalog::type_text(&t.columns[c])
                )));
            }
            if t.partition_of.is_some() {
                return Err(Error::feature(
                    "an expiry is set on the partitioned table, not on a partition",
                ));
            }
            Some(c)
        }
    };
    catalog::update_table(txn, &t)
}

/// Deletes the expired rows of every table with an expiry — or of
/// `table` — at `now` (µs since 2000-01-01 UTC): each table in a
/// transaction of its own, as the system (row-level security does not
/// hide a row from its expiry), tried again on a conflict.  The rows
/// deleted, in all.
pub fn expire(kv: &Arc<dyn Kv>, reg: &Registry, table: Option<&str>, now: i64) -> Result<u64> {
    let tables = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
    };
    let mut deleted = 0;
    for t in tables {
        let Some(c) = t.expiry else { continue };
        if table.is_some_and(|n| n != t.name) {
            continue;
        }
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let column = &t.columns[c];
        // The cutoff in the column's own type.
        let cutoff = match column.ty {
            DataType::Date => "CAST($1 AS date)".to_string(),
            DataType::Timestamp => "CAST($1 AS timestamp)".to_string(),
            _ => "$1".to_string(),
        };
        let sql = format!(
            "DELETE FROM {} WHERE {} <= {cutoff}",
            q(&t.name),
            q(&column.name)
        );
        let stmt = crate::parser::parse(&sql)?.remove(0);
        let mut attempt = 0;
        loop {
            let mut txn = kv.begin(TxnOptions::default())?;
            let r = crate::datetime::with_now(now, kv, || {
                crate::session::execute(
                    txn.as_mut(),
                    reg,
                    &stmt,
                    &[Value::TimestampTz(now)],
                    &mut crate::session::Collect::default(),
                )
            });
            let r = r.and_then(|out| {
                txn.commit()?;
                Ok(out)
            });
            match r {
                Ok(crate::session::Output::Command(tag)) => {
                    deleted += tag
                        .rsplit(' ')
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0);
                    break;
                }
                Ok(_) => break,
                // Dropped meanwhile: nothing to expire.
                Err(e) if e.code == "42P01" => break,
                Err(e) if e.code == "40001" && attempt < 20 => {
                    attempt += 1;
                    kv.back_off(attempt);
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(deleted)
}
