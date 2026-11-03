//! `DISTINCT` inside aggregate calls planned as a grouping (PLAN.md
//! §M14.20 W6): `SELECT g, count(DISTINCT x), sum(y) FROM t GROUP BY g`
//! is aggregated twice —
//!
//! 1. grouped by `g, x`, each other aggregate giving its partial state
//!    ([`GroupAccumulator::partial`]) instead of its result;
//! 2. those rows grouped by `g`: the `DISTINCT` aggregate fed `x` once a
//!    group (each `x` is in one row of a `g`), the others combining the
//!    partial states ([`GroupAccumulator::combine`]).
//!
//! Both are ordinary groupings, so the vectorized, parallel one runs them
//! within `work_mem` — where a `DISTINCT` call was a set of values a
//! group, kept a row at a time on one thread.  It holds when:
//!
//! - the `DISTINCT` calls all have the same arguments, and no call has an
//!   `ORDER BY`;
//! - every aggregate's result does not depend on the order of its rows
//!   ([`Aggregate::order_free`]): the second grouping feeds them in
//!   another order than the rows came in;
//! - every other aggregate has partial states ([`GroupAccumulator::
//!   combinable`]).
//! - the input is a pipeline (a scan through filters and projections):
//!   the first grouping then runs vectorized and on the workers — over a
//!   join's rows, a row at a time, it would only hash each row twice
//!   (TPC-H Q16, 127 → 192 ms).
//!
//! The result is the same: the first grouping's groups are those of
//! `g, x` in the order first seen, and so the second's, and errors come
//! from the same expressions over the same rows.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{Accumulator, AggRef, Aggregate, ArgInfo, GroupAccumulator, Signature};
use crate::plan::{Agg, Plan};
use crate::types::Value;
use crate::vector::Vector;

/// The plan with its eligible `DISTINCT` aggregate calls planned as two
/// groupings.
pub(crate) fn two_level(plan: Plan) -> Plan {
    let plan = crate::optimizer::map_children(plan, &two_level);
    let Plan::Aggregate { input, group, aggs } = plan else {
        return plan;
    };
    if !eligible(&aggs) || !crate::vector::pipelined(&input) {
        return Plan::Aggregate { input, group, aggs };
    }
    let dargs = aggs
        .iter()
        .find(|a| a.distinct)
        .map(|a| a.args.clone())
        .expect("a DISTINCT call");
    let (n, d) = (group.len(), dargs.len());
    let mut partials = Vec::new();
    let mut outer = Vec::new();
    for a in aggs {
        if a.distinct {
            outer.push(Agg {
                func: a.func,
                args: (n..n + d).map(Expr::Column).collect(),
                distinct: false,
                order: Vec::new(),
            });
        } else {
            outer.push(Agg {
                func: AggRef::new(Arc::new(Combined::new(a.func.clone()))),
                args: vec![Expr::Column(n + d + partials.len())],
                distinct: false,
                order: Vec::new(),
            });
            partials.push(Agg {
                func: AggRef::new(Arc::new(Partial::new(a.func))),
                args: a.args,
                distinct: false,
                order: Vec::new(),
            });
        }
    }
    let inner = Plan::Aggregate {
        input,
        group: group.into_iter().chain(dargs).collect(),
        aggs: partials,
    };
    Plan::Aggregate {
        input: Box::new(inner),
        group: (0..n).map(Expr::Column).collect(),
        aggs: outer,
    }
}

/// Whether `aggs` can be aggregated in two groupings (the module's
/// conditions).
fn eligible(aggs: &[Agg]) -> bool {
    let Some(first) = aggs.iter().find(|a| a.distinct) else {
        return false;
    };
    !first.args.is_empty()
        && aggs.iter().all(|a| {
            a.order.is_empty()
                && a.func.order_free()
                && if a.distinct {
                    a.args == first.args
                } else {
                    a.func.grouped().is_some_and(|g| g.combinable())
                }
        })
}

/// An aggregate whose result is another's partial state (the first
/// grouping's).
struct Partial {
    inner: AggRef,
    name: String,
}

impl Partial {
    fn new(inner: AggRef) -> Self {
        let name = format!("partial {}", inner.name());
        Partial { inner, name }
    }
}

/// An aggregate fed another's partial states, giving its result (the
/// second grouping's).
struct Combined {
    inner: AggRef,
    name: String,
}

impl Combined {
    fn new(inner: AggRef) -> Self {
        let name = format!("combined {}", inner.name());
        Combined { inner, name }
    }
}

fn no_call() -> Error {
    Error::internal("an aggregate of the planner's, not to be called")
}

impl Aggregate for Partial {
    fn name(&self) -> &str {
        &self.name
    }

    fn arity(&self) -> RangeInclusive<usize> {
        self.inner.arity()
    }

    fn accepts_star(&self) -> bool {
        self.inner.accepts_star()
    }

    fn keeps_nulls(&self) -> bool {
        self.inner.keeps_nulls()
    }

    fn infallible(&self) -> bool {
        self.inner.infallible()
    }

    fn order_free(&self) -> bool {
        true
    }

    fn resolve(&self, _args: &[ArgInfo]) -> Result<Signature> {
        Err(no_call())
    }

    fn start(&self) -> Box<dyn Accumulator> {
        let mut groups = self.grouped().expect("combinable");
        groups.resize(1);
        Box::new(One {
            groups,
            combine: false,
        })
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(PartialGroups(self.inner.grouped()?)))
    }
}

impl Aggregate for Combined {
    fn name(&self) -> &str {
        &self.name
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }

    fn infallible(&self) -> bool {
        self.inner.infallible()
    }

    fn order_free(&self) -> bool {
        true
    }

    fn resolve(&self, _args: &[ArgInfo]) -> Result<Signature> {
        Err(no_call())
    }

    fn start(&self) -> Box<dyn Accumulator> {
        let mut groups = self.grouped().expect("combinable");
        groups.resize(1);
        Box::new(One {
            groups,
            combine: true,
        })
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(CombinedGroups(self.inner.grouped()?)))
    }
}

/// One group's state, for the row-at-a-time path: a group of the
/// aggregate's [`GroupAccumulator`] — fed rows (a [`Partial`]'s, giving
/// its partial state) or partial states (a [`Combined`]'s).
struct One {
    groups: Box<dyn GroupAccumulator>,
    combine: bool,
}

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if self.combine {
            return self
                .groups
                .combine(0, args.first().cloned().unwrap_or(Value::Null));
        }
        let vectors: Vec<Vector> = args.iter().map(|v| Vector::Const(v.clone())).collect();
        let refs: Vec<&Vector> = vectors.iter().collect();
        self.groups.update(&[0], &[0], &refs)
    }

    fn finish(&mut self) -> Result<Value> {
        if self.combine {
            self.groups.finish(0)
        } else {
            self.groups.partial(0)
        }
    }
}

/// [`Partial`]'s groups: the aggregate's, each finished as its partial
/// state.
struct PartialGroups(Box<dyn GroupAccumulator>);

impl GroupAccumulator for PartialGroups {
    fn resize(&mut self, groups: usize) {
        self.0.resize(groups);
    }

    fn group_bytes(&self) -> usize {
        self.0.group_bytes()
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        self.0.update(rows, groups, args)
    }

    fn update_all(
        &mut self,
        len: usize,
        group: u32,
        skip_nulls: bool,
        args: &[&Vector],
    ) -> Result<()> {
        self.0.update_all(len, group, skip_nulls, args)
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        self.0.partial(group)
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        self.0.combine(group, partial)
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.0.partial(group)
    }
}

/// [`Combined`]'s groups: the aggregate's, fed partial states.
struct CombinedGroups(Box<dyn GroupAccumulator>);

impl GroupAccumulator for CombinedGroups {
    fn resize(&mut self, groups: usize) {
        self.0.resize(groups);
    }

    fn group_bytes(&self) -> usize {
        self.0.group_bytes()
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let Some(states) = args.first() else {
            return Err(no_call());
        };
        for (&r, &g) in rows.iter().zip(groups) {
            self.0.combine(g as usize, states.value(r as usize))?;
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        self.0.partial(group)
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        self.0.combine(group, partial)
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.0.finish(group)
    }
}
