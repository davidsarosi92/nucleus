//! A read by key's plan kept by its shape (PLAN.md §M14.33 R2).
//!
//! A session keeps a query's plan by its text from the text's second
//! coming ([`crate::by_text`]); a read by key with its key inline —
//! `SELECT s FROM b WHERE id = 42` — is a new text for every key, so it
//! was bound and planned every time: 16.5 of its 23.6 µs (R1).  Here
//! such a read becomes its shape — `… WHERE id = $1` — and the key a
//! parameter, so one plan serves every key, kept by the shape's text.
//!
//! Only where the plan cannot depend on the value: SQL Server's *simple
//! parameterization* rule (a "safe" plan, the same for every value).  A
//! `SELECT` of one stored table, not partitioned, its `WHERE` only
//! equalities of a column and a literal, which name every column of the
//! primary key: at most one row, found by the key, whatever the key is.
//! The literals become parameters only where the parameter carries the
//! same value the literal would: an integer for an integer column it
//! fits, a string for a text column.  Anything else is planned as it
//! comes.

use crate::ast::{BinaryOp, Expr, Query, SetExpr, TableRef};
use crate::types::{DataType, Value};
use nucleus_kv::KvTxn;

/// A query made its shape: the literals the parameters took, in order.
pub(crate) struct Shaped {
    pub query: Query,
    pub values: Vec<Value>,
}

/// `q` as its shape, when it is a read by its table's whole primary key.
pub(crate) fn by_key(txn: &dyn KvTxn, q: &Query) -> Option<Shaped> {
    if q.with.is_some() {
        return None;
    }
    let SetExpr::Select(s) = &q.body else {
        return None;
    };
    if s.from.len() != 1 || !s.group_by.is_empty() || s.having.is_some() || !s.windows.is_empty() {
        return None;
    }
    let TableRef::Table {
        schema: None,
        name,
        alias,
        sample: None,
        as_of: None,
        ..
    } = &s.from[0]
    else {
        return None;
    };
    let mut eqs = Vec::new();
    equalities(s.filter.as_ref()?, &mut eqs)?;
    let table = crate::catalog::get_table(txn, name).ok()??;
    if table.primary_key.is_empty() || table.partition_by.is_some() || table.partition_of.is_some()
    {
        return None;
    }
    let mut keyed = vec![false; table.columns.len()];
    for (qualifier, column, value) in &eqs {
        if qualifier
            .as_ref()
            .is_some_and(|t| t != alias.as_ref().unwrap_or(name))
        {
            return None;
        }
        let at = table.columns.iter().position(|c| c.name == **column)?;
        if !carries(table.columns[at].ty, value) {
            return None;
        }
        keyed[at] = true;
    }
    if !table.primary_key.iter().all(|&k| keyed[k]) {
        return None;
    }
    let mut query = q.clone();
    let SetExpr::Select(s) = &mut query.body else {
        unreachable!("a select, as above")
    };
    let mut values = Vec::with_capacity(eqs.len());
    parameterize(s.filter.as_mut().expect("a filter, as above"), &mut values);
    Some(Shaped { query, values })
}

/// The equalities of a column and a literal that `e` is the conjunction
/// of — (qualifier, column, literal) — or `None` if it holds anything
/// else.
fn equalities<'a>(
    e: &'a Expr,
    out: &mut Vec<(&'a Option<String>, &'a String, &'a Value)>,
) -> Option<()> {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            equalities(left, out)?;
            equalities(right, out)
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => match (&**left, &**right) {
            (Expr::Column { table, name }, Expr::Literal(v))
            | (Expr::Literal(v), Expr::Column { table, name }) => {
                out.push((table, name, v));
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether a parameter holding `v`, typed as a column of type `ty`, means
/// what the literal did there.
fn carries(ty: DataType, v: &Value) -> bool {
    match (ty, v) {
        (DataType::SmallInt, Value::Int(i)) => i16::try_from(*i).is_ok(),
        (DataType::Integer, Value::Int(i)) => i32::try_from(*i).is_ok(),
        (DataType::BigInt, Value::Int(_)) => true,
        (DataType::Text, Value::Text(_)) => true,
        _ => false,
    }
}

/// The literals of `e`'s equalities (as [`equalities`] found them, in the
/// same order) made parameters `$1`, `$2`, …; their values to `values`.
fn parameterize(e: &mut Expr, values: &mut Vec<Value>) {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            parameterize(left, values);
            parameterize(right, values);
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            for side in [left, right] {
                if let Expr::Literal(v) = &mut **side {
                    values.push(std::mem::replace(v, Value::Null));
                    **side = Expr::Param(values.len());
                }
            }
        }
        _ => {}
    }
}
