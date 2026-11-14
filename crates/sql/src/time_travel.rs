//! Time travel (PLAN.md §M14.14): what a past snapshot is named by —
//! `AS OF TIMESTAMP expr` (the last commit at or before a time), `AS OF
//! COMMIT expr` — for a transaction, a session (`read_as_of`) or a table
//! reference.

use std::sync::Arc;

use nucleus_kv::{Kv, Timestamp, TxnOptions};

use crate::ast::{self, AsOf};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::session::{Collect, Output, execute_statement};
use crate::types::{DataType, Value};

/// The snapshot `as_of` names: its expression evaluated now, as a one-row
/// query in a transaction of its own (`now() - interval '1 hour'`; no
/// columns, no parameters) — a time to the last commit at or before it
/// (never one in the future: `22023`), a commit to its own snapshot.
/// `72000` for a time no commit known is at or before.
pub fn snapshot(kv: &Arc<dyn Kv>, registry: &Registry, as_of: &AsOf) -> Result<Timestamp> {
    let (expr, ty) = match as_of {
        AsOf::Time(e) => (e, DataType::TimestampTz),
        AsOf::Commit(e) => (e, DataType::BigInt),
    };
    let query = ast::Statement::Query(ast::Query {
        with: None,
        body: ast::SetExpr::Select(Box::new(ast::Select {
            distinct: false,
            items: vec![ast::SelectItem::Expr {
                expr: ast::Expr::Cast {
                    expr: Box::new(expr.clone()),
                    ty: ast::SqlType::builtin(ty),
                    typmod: ast::TypeMod::default(),
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
    });
    let mut txn = kv.begin(TxnOptions::read_only())?;
    let now = crate::datetime::from_system(kv.wall_clock());
    let mut all = Collect::default();
    let out = crate::datetime::with_now(now, kv, || {
        execute_statement(txn.as_mut(), registry, &query, &[], &mut all)
    })?;
    let rows = match out {
        Output::Rows { rows, .. } => rows,
        Output::Command(_) => all.rows,
    };
    match rows.into_iter().next().and_then(|r| r.into_iter().next()) {
        Some(Value::TimestampTz(t)) => {
            if t > now {
                return Err(Error::invalid_parameter("AS OF names a time in the future"));
            }
            let micros = u64::try_from(t + crate::datetime::UNIX_TO_PG_MICROS).unwrap_or(0);
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_micros(micros);
            Ok(kv.snapshot_at(time)?)
        }
        Some(Value::Int(n)) if n > 0 => n
            .to_string()
            .parse()
            .map_err(|_| Error::invalid_parameter("AS OF COMMIT names no commit")),
        _ => Err(Error::invalid_parameter(
            "AS OF names no time or commit (NULL, or not positive)",
        )),
    }
}

/// `snapshot_commit()`: the commit the statement reads — its
/// transaction's snapshot (a table read `AS OF`, that one's) — so what it
/// read is read again by `AS OF COMMIT n` while the history keeps it
/// (PLAN.md §M14.24 R5: a citation's version).
struct SnapshotCommit;

impl crate::ext::ScalarFunction for SnapshotCommit {
    fn name(&self) -> &str {
        "snapshot_commit"
    }

    fn arity(&self) -> std::ops::RangeInclusive<usize> {
        0..=0
    }

    fn resolve(&self, _: &[crate::ext::ArgInfo]) -> Result<crate::ext::Signature> {
        Ok(crate::ext::Signature::new(
            vec![],
            crate::ext::Returns::Type(Some(DataType::BigInt)),
        ))
    }

    fn volatility(&self) -> crate::ext::Volatility {
        crate::ext::Volatility::Stable
    }

    fn call(&self, cx: &crate::ext::CallContext<'_>, _: &[Value]) -> Result<Value> {
        let ts = cx.txn().read_ts().to_string();
        ts.parse()
            .map(Value::Int)
            .map_err(|_| Error::internal("a snapshot's commit"))
    }
}

/// Registers `snapshot_commit()`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["snapshot_commit"], Arc::new(SnapshotCommit))
}
