//! Row constructors — `(a, b, …)`, `ROW(…)` — compared (SQL's row value
//! expressions, issue #16): rewritten, before binding, to comparisons of
//! their fields with PostgreSQL's meaning, `NULL`s included.
//!
//! - `=`: every pair equal (`AND`: false if a pair is, else `NULL` if a
//!   pair is); `<>`: a pair unequal (`OR`).
//! - `<`, `<=`, `>`, `>=`: the first pair not equal decides —
//!   `a1 < b1 OR (a1 = b1 AND (a2 < b2 OR …))`, the last pair compared
//!   with the operator itself; a `NULL` pair before the deciding one makes
//!   it `NULL`, as the `AND`s and `OR`s do.  The first pair's bound
//!   (`a1 <= b1`) is put before it: implied, it changes no result, and it
//!   lets an index on the first column bound the scan (keyset paging,
//!   `WHERE (k1, k2) > ($1, $2)`).
//! - `IS [NOT] DISTINCT FROM`: a pair distinct; `[NOT] IN (rows…)`: equal
//!   to one; `IS NULL`: every field `NULL`, `IS NOT NULL`: none.
//!
//! Anywhere else a row is a value of its own, a `record` ([`record`]): its
//! fields, compared field by field (a `NULL` after the others and equal to
//! a `NULL`, as PostgreSQL's record values are — and as `DISTINCT`,
//! grouping and `ORDER BY` want), written as PostgreSQL's `(a,b)`; so is a
//! row constructor compared with a record that is not one.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::ast::{BinaryOp, Expr, UnaryOp};
use crate::error::{Error, Result};
use crate::ext::{ArgInfo, CallContext, FnRef, ScalarFunction, Signature, Volatility};
use crate::types::Value;

/// The `record` of `fields` (bound).
pub(crate) fn record(fields: Vec<crate::expr::Expr>) -> crate::expr::Expr {
    crate::expr::Expr::Call(FnRef::new(Arc::new(Record)), fields)
}

struct Record;

impl ScalarFunction for Record {
    fn name(&self) -> &str {
        "row"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        0..=usize::MAX
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Err(Error::internal("ROW(…) is bound by the binder"))
    }

    /// `ROW(NULL, NULL)` is a record, not `NULL`.
    fn strict(&self) -> bool {
        false
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        Ok(Value::Record(args.into()))
    }
}

/// `e` with its row comparison rewritten, if it is one.
pub(crate) fn expand(e: &Expr) -> Result<Option<Expr>> {
    Ok(Some(match e {
        Expr::Binary { op, left, right } if is_row(left) && is_row(right) => {
            if !matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            ) {
                return Err(unsupported());
            }
            let (l, r) = pairs(left, right)?;
            compare(*op, &l, &r)
        }
        Expr::Function { name, args, .. }
            if name == "__is_distinct_from" && args.iter().all(is_row) =>
        {
            let (l, r) = pairs(&args[0], &args[1])?;
            any(l
                .iter()
                .zip(&r)
                .map(|(a, b)| distinct_from(e, a, b))
                .collect())
        }
        Expr::IsNull { expr, negated } if is_row(expr) => {
            let Expr::Row(fields) = &**expr else {
                unreachable!()
            };
            all(fields
                .iter()
                .map(|f| Expr::IsNull {
                    expr: Box::new(f.clone()),
                    negated: *negated,
                })
                .collect())
        }
        Expr::InList {
            expr,
            list,
            negated,
        } if is_row(expr) && list.iter().all(is_row) => {
            let mut equal = Vec::with_capacity(list.len());
            for item in list {
                let (l, r) = pairs(expr, item)?;
                equal.push(compare(BinaryOp::Eq, &l, &r));
            }
            let found = any(equal);
            if *negated { not(found) } else { found }
        }
        Expr::AnyAll { left: expr, .. } if is_row(expr) => {
            return Err(unsupported());
        }
        _ => return Ok(None),
    }))
}

/// `row [NOT] IN (query)`, `query` of as many columns as `row` has
/// fields: with the row comparison's meaning — true if a row of it is
/// equal, else `NULL` if one might be (a comparison `NULL`), else false:
///
/// `CASE WHEN EXISTS (SELECT 1 FROM (query) __in(c…) WHERE row = (c…))
/// THEN true WHEN EXISTS (… WHERE (row = (c…)) IS NULL) THEN NULL ELSE
/// false END` — the second looked at only when the first finds nothing.
pub(crate) fn in_subquery(row: &[Expr], query: &crate::ast::Query, negated: bool) -> Expr {
    use crate::ast::{Query, Select, SelectItem, SetExpr, TableRef};
    const ALIAS: &str = "__in";
    let columns: Vec<String> = (1..=row.len()).map(|i| format!("__in{i}")).collect();
    let theirs = Expr::Row(
        columns
            .iter()
            .map(|c| Expr::Column {
                table: Some(ALIAS.into()),
                name: c.clone(),
            })
            .collect(),
    );
    let equal = binary(BinaryOp::Eq, Expr::Row(row.to_vec()), theirs);
    let exists = |filter: Expr| Expr::Exists {
        query: Box::new(Query {
            with: None,
            body: SetExpr::Select(Box::new(Select {
                distinct: false,
                items: vec![SelectItem::Expr {
                    expr: Expr::Literal(Value::Int(1)),
                    alias: None,
                }],
                from: vec![TableRef::Subquery {
                    query: Box::new(query.clone()),
                    alias: ALIAS.into(),
                    columns: columns.clone(),
                }],
                filter: Some(filter),
                group_by: vec![],
                grouping_sets: None,
                having: None,
                windows: vec![],
            })),
            order_by: vec![],
            limit: None,
            offset: None,
        }),
        negated: false,
    };
    let found = Expr::Case {
        operand: None,
        whens: vec![
            (exists(equal.clone()), Expr::Literal(Value::Bool(true))),
            (
                exists(Expr::IsNull {
                    expr: Box::new(equal),
                    negated: false,
                }),
                Expr::Literal(Value::Null),
            ),
        ],
        otherwise: Some(Box::new(Expr::Literal(Value::Bool(false)))),
    };
    if negated { not(found) } else { found }
}

/// The error for a row compared with what is not a row constructor.
pub(crate) fn unsupported() -> Error {
    Error::feature(
        "a row is compared with another row constructor (=, <>, <, <=, >, >=, IS [NOT] DISTINCT FROM, IN), with IN (subquery), or tested with IS [NOT] NULL; not with = ANY / ALL (subquery) yet",
    )
}

fn is_row(e: &Expr) -> bool {
    matches!(e, Expr::Row(_))
}

/// The fields of two rows, as many on each side.
fn pairs(left: &Expr, right: &Expr) -> Result<(Vec<Expr>, Vec<Expr>)> {
    let (Expr::Row(l), Expr::Row(r)) = (left, right) else {
        return Err(unsupported());
    };
    if l.len() != r.len() {
        return Err(Error::syntax(
            "unequal number of entries in row expressions",
        ));
    }
    if l.is_empty() {
        return Err(Error::feature("cannot compare rows of zero length"));
    }
    Ok((l.clone(), r.clone()))
}

fn compare(op: BinaryOp, l: &[Expr], r: &[Expr]) -> Expr {
    let pair = |op, i: usize| binary(op, l[i].clone(), r[i].clone());
    match op {
        BinaryOp::Eq => all((0..l.len()).map(|i| pair(BinaryOp::Eq, i)).collect()),
        BinaryOp::NotEq => any((0..l.len()).map(|i| pair(BinaryOp::NotEq, i)).collect()),
        _ => {
            let (strict, weak) = match op {
                BinaryOp::Lt | BinaryOp::LtEq => (BinaryOp::Lt, BinaryOp::LtEq),
                _ => (BinaryOp::Gt, BinaryOp::GtEq),
            };
            let last = l.len() - 1;
            let mut x = pair(op, last);
            for i in (0..last).rev() {
                x = binary(
                    BinaryOp::Or,
                    pair(strict, i),
                    binary(BinaryOp::And, pair(BinaryOp::Eq, i), x),
                );
            }
            if last == 0 {
                x
            } else {
                binary(BinaryOp::And, pair(weak, 0), x)
            }
        }
    }
}

/// `a IS DISTINCT FROM b`, as the call `e` is.
fn distinct_from(e: &Expr, a: &Expr, b: &Expr) -> Expr {
    let Expr::Function {
        name,
        distinct,
        star,
        order_by,
        within_group,
        filter,
        ..
    } = e
    else {
        unreachable!()
    };
    Expr::Function {
        name: name.clone(),
        args: vec![a.clone(), b.clone()],
        distinct: *distinct,
        star: *star,
        order_by: order_by.clone(),
        within_group: *within_group,
        filter: filter.clone(),
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn not(e: Expr) -> Expr {
    Expr::Unary {
        op: UnaryOp::Not,
        expr: Box::new(e),
    }
}

/// `x1 AND x2 AND …` (not empty).
fn all(xs: Vec<Expr>) -> Expr {
    fold(BinaryOp::And, xs)
}

/// `x1 OR x2 OR …` (not empty).
fn any(xs: Vec<Expr>) -> Expr {
    fold(BinaryOp::Or, xs)
}

fn fold(op: BinaryOp, xs: Vec<Expr>) -> Expr {
    let mut it = xs.into_iter();
    let first = it.next().expect("not empty");
    it.fold(first, |acc, x| binary(op, acc, x))
}
