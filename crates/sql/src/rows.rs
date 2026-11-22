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
//! A row is not a value of its own yet (a record type): one anywhere else
//! is `0A000`.

use crate::ast::{BinaryOp, Expr, UnaryOp};
use crate::error::{Error, Result};

/// `e` with its row comparison rewritten, if it is one.
pub(crate) fn expand(e: &Expr) -> Result<Option<Expr>> {
    Ok(Some(match e {
        Expr::Binary { op, left, right } if is_row(left) || is_row(right) => {
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
            if name == "__is_distinct_from" && args.iter().any(is_row) =>
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
        } if is_row(expr) || list.iter().any(is_row) => {
            let mut equal = Vec::with_capacity(list.len());
            for item in list {
                let (l, r) = pairs(expr, item)?;
                equal.push(compare(BinaryOp::Eq, &l, &r));
            }
            let found = any(equal);
            if *negated { not(found) } else { found }
        }
        Expr::InSubquery { expr, .. } | Expr::AnyAll { left: expr, .. } if is_row(expr) => {
            return Err(unsupported());
        }
        _ => return Ok(None),
    }))
}

/// The error for a row anywhere but in a comparison of rows.
pub(crate) fn unsupported() -> Error {
    Error::feature(
        "a row value is supported compared with another row (=, <>, <, <=, >, >=, IS [NOT] DISTINCT FROM, IN), or tested with IS [NOT] NULL; not as a value of its own (a record type) or against a subquery yet",
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
