//! `GROUP BY GROUPING SETS (…) | ROLLUP (…) | CUBE (…)` (SQL:1999, issue
//! #16): one grouping per set, over the same input, their rows together.
//!
//! The parser turns the clause into the list of sets, each the positions
//! of its keys among all the keys named; the binder binds the keys once,
//! with one more key after them, the set's mask — the bits of the keys a
//! row's set leaves out (`GROUPING(…)` reads it) —, and binds the select
//! list, `HAVING` and `ORDER BY` over that layout as over one grouping.
//! [`plan`] then makes it a `UNION ALL` of a grouping per set, each giving
//! the layout: its keys where the set has them, `NULL` where not, the
//! mask a constant, then the aggregates.  So the rest of the plan, the
//! executors and the optimizer see ordinary groupings — a filter on
//! `GROUPING(…)` above folds to a constant in each branch.
//!
//! Each set reads the input again; PostgreSQL's sorted rollup computes a
//! chain of sets in one pass — for the performance pass.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::ast::SetOp;
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{ArgInfo, CallContext, FnRef, ScalarFunction, Signature, Volatility};
use crate::plan::{Agg, Plan};
use crate::types::Value;

/// The most sets a clause may make (PostgreSQL's limit).
pub const MOST_SETS: usize = 4096;

/// The most keys a clause with sets may name: a mask's bits.
pub const MOST_KEYS: usize = 63;

/// The mask key's place holder among the bound keys: never evaluated —
/// [`plan`] puts each set's mask in its place.
pub(crate) const MASK: Expr = Expr::Param(usize::MAX);

/// The grouping of each of `sets` (positions among `group`'s keys, the
/// last of which is [`MASK`]) over `input`, their rows together.
pub(crate) fn plan(input: Plan, mut group: Vec<Expr>, aggs: Vec<Agg>, sets: &[Vec<usize>]) -> Plan {
    debug_assert_eq!(group.last(), Some(&MASK));
    group.pop();
    let n = group.len();
    let mut out: Option<Plan> = None;
    for set in sets {
        let grouped = Plan::Aggregate {
            input: Box::new(input.clone()),
            group: set.iter().map(|&i| group[i].clone()).collect(),
            aggs: aggs.clone(),
        };
        let mut mask = 0i64;
        let mut exprs = Vec::with_capacity(n + 1 + aggs.len());
        for i in 0..n {
            match set.iter().position(|&j| j == i) {
                Some(p) => exprs.push(Expr::Column(p)),
                None => {
                    mask |= 1 << i;
                    exprs.push(Expr::Literal(Value::Null));
                }
            }
        }
        exprs.push(Expr::Literal(Value::Int(mask)));
        exprs.extend((0..aggs.len()).map(|a| Expr::Column(set.len() + a)));
        let branch = Plan::Project {
            input: Box::new(grouped),
            exprs,
        };
        out = Some(match out {
            None => branch,
            Some(left) => Plan::SetOp {
                op: SetOp::Union,
                left: Box::new(left),
                right: Box::new(branch),
                all: true,
            },
        });
    }
    out.expect("a set at least")
}

/// `GROUPING(k1, …, km)`: over the mask column `mask`, a bit per argument
/// (the first the highest), set when the row's set leaves that key out —
/// `keys` the arguments' positions among the keys.
pub(crate) fn grouping_call(mask: usize, keys: &[usize]) -> Expr {
    let mut args = vec![Expr::Column(mask)];
    args.extend(keys.iter().map(|&k| Expr::Literal(Value::Int(k as i64))));
    Expr::Call(FnRef::new(Arc::new(Grouping)), args)
}

struct Grouping;

impl ScalarFunction for Grouping {
    fn name(&self) -> &str {
        "grouping"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=usize::MAX
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Err(Error::internal("GROUPING is bound by the binder"))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let Value::Int(mask) = args[0] else {
            return Err(Error::internal("a grouping set's mask"));
        };
        let m = args.len() - 1;
        let mut out = 0i64;
        for (j, k) in args[1..].iter().enumerate() {
            let Value::Int(k) = k else {
                return Err(Error::internal("a grouping key's position"));
            };
            out |= ((mask >> k) & 1) << (m - 1 - j);
        }
        Ok(Value::Int(out))
    }
}
