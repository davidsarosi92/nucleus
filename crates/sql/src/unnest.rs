//! Subqueries unnested into joins (PLAN.md §M14.20 V10): a `WHERE` (or
//! scan filter) conjunct `EXISTS (…)`, `NOT EXISTS (…)` or `x IN (…)`
//! becomes a semi or anti join of the rows it filters with the subquery's
//! rows — one pass over each, hashed on the correlation's equalities —
//! instead of the subquery run once per row.
//!
//! A subquery unnests when its rows do not depend on the row it runs for
//! (uncorrelated — `IN` only: an uncorrelated `EXISTS` is one value, run
//! once), or when it reads one table (under projections, `DISTINCT` and
//! sorts, which cannot change whether a row exists) and refers to the row
//! only in its filter's top-level conjuncts: those become the join
//! condition, the rest stays its filter.  `NOT IN` stays a subquery (its
//! NULLs differ from an anti join's).  The right side keeps only the
//! columns the condition reads, so a join holds little of each row.

use crate::ast::{BinaryOp, UnaryOp};
use crate::binder::{conjunction, conjuncts};
use crate::expr::{Expr, SubKind, SubPlan};
use crate::plan::{Access, JoinAlgo, JoinType, Plan};

/// `plan` with its unnestable subquery conjuncts as joins.
pub fn unnest(mut plan: Plan) -> Plan {
    for c in plan.children_mut() {
        let child = std::mem::replace(c, placeholder());
        *c = unnest(child);
    }
    match plan {
        Plan::Filter { input, predicate } => {
            let (input, rest) = joins(*input, predicate);
            match conjunction(rest) {
                Some(predicate) => Plan::Filter {
                    input: Box::new(input),
                    predicate,
                },
                None => input,
            }
        }
        Plan::Scan {
            table,
            access,
            filter: Some(filter),
            estimate,
        } if has_candidate(&filter) => {
            let scan = Plan::Scan {
                table,
                access,
                filter: None,
                estimate,
            };
            let (input, rest) = joins(scan, filter);
            match conjunction(rest) {
                Some(predicate) => Plan::Filter {
                    input: Box::new(input),
                    predicate,
                },
                None => input,
            }
        }
        other => other,
    }
}

fn placeholder() -> Plan {
    Plan::Values {
        rows: Vec::new(),
        width: 0,
    }
}

fn has_candidate(e: &Expr) -> bool {
    let mut parts = Vec::new();
    conjuncts(e.clone(), &mut parts);
    parts.iter().any(|p| candidate(p).is_some())
}

/// The subquery of a conjunct that may unnest, and the join it would be.
fn candidate(p: &Expr) -> Option<(&SubPlan, JoinType)> {
    match p {
        Expr::Subquery(s) if matches!(s.kind, SubKind::Exists) => Some((s, JoinType::Semi)),
        Expr::Subquery(s) if matches!(s.kind, SubKind::In(_, false)) => Some((s, JoinType::Semi)),
        Expr::Unary(UnaryOp::Not, e) => match &**e {
            Expr::Subquery(s) if matches!(s.kind, SubKind::Exists) => Some((s, JoinType::Anti)),
            _ => None,
        },
        _ => None,
    }
}

/// `input` joined with each unnestable subquery of `predicate`'s
/// conjuncts; the conjuncts left.
fn joins(mut input: Plan, predicate: Expr) -> (Plan, Vec<Expr>) {
    let mut parts = Vec::new();
    conjuncts(predicate, &mut parts);
    let mut rest = Vec::new();
    for p in parts {
        let lw = input.width();
        match candidate(&p).and_then(|(s, kind)| right_side(s, lw).map(|r| (r, kind))) {
            // (Placed among the joins once they are ordered: `optimizer`.)
            Some(((right, on), kind)) => input = semi(input, unnest(right), on, kind),
            None => rest.push(p),
        }
    }
    (input, rest)
}

/// The semi or anti join of `input` with `right` on `on`, moved down among
/// `input`'s inner joins (through the projections that reorder columns, and
/// the semi and anti joins already there) as far as the columns of `input`
/// it reads allow — `(A ⋈ B) ⋉ R` is `(A ⋉ R) ⋈ B` when the condition
/// reads only `A` — and while the rows it would meet there are no more
/// than here (`rows`: an estimate; `None` without statistics, when it goes
/// as deep as it can): a subquery that filters one table filters it before
/// the joins that multiply it, after those that narrow it.
pub(crate) fn place(
    input: Plan,
    right: Plan,
    on: Expr,
    kind: JoinType,
    rows: &dyn Fn(&Plan) -> Option<f64>,
) -> Plan {
    let lw = input.width();
    let reads: Vec<usize> = on.reads().into_iter().filter(|&c| c < lw).collect();
    let here = rows(&input);
    let fewer = |child: &Plan| match (here, rows(child)) {
        (Some(h), Some(c)) => c <= h,
        _ => true,
    };
    match input {
        Plan::Join {
            left,
            right: other,
            kind: inner @ JoinType::Inner,
            on: cond,
            algo,
        } if !reads.is_empty() => {
            let ll = left.width();
            if reads.iter().all(|&c| c < ll) && fewer(&left) {
                let on = on.map_columns(&|c| Expr::Column(if c >= lw { c - lw + ll } else { c }));
                return Plan::Join {
                    left: Box::new(place(*left, right, on, kind, rows)),
                    right: other,
                    kind: inner,
                    on: cond,
                    algo,
                };
            }
            if reads.iter().all(|&c| c >= ll) && fewer(&other) {
                let rw = other.width();
                let on =
                    on.map_columns(&|c| Expr::Column(if c >= lw { c - lw + rw } else { c - ll }));
                return Plan::Join {
                    left,
                    right: Box::new(place(*other, right, on, kind, rows)),
                    kind: inner,
                    on: cond,
                    algo,
                };
            }
            let input = Plan::Join {
                left,
                right: other,
                kind: inner,
                on: cond,
                algo,
            };
            semi(input, right, on, kind)
        }
        // A projection of columns: below it, the columns it takes.
        Plan::Project {
            input: below,
            exprs,
        } if !reads.is_empty() && reads.iter().all(|&c| matches!(exprs[c], Expr::Column(_))) => {
            let bw = below.width();
            let on = on.map_columns(&|c| {
                if c >= lw {
                    Expr::Column(c - lw + bw)
                } else {
                    exprs[c].clone()
                }
            });
            Plan::Project {
                input: Box::new(place(*below, right, on, kind, rows)),
                exprs,
            }
        }
        // A semi or anti join keeps its left's columns: below it too.
        Plan::Join {
            left,
            right: other,
            kind: k @ (JoinType::Semi | JoinType::Anti),
            on: cond,
            algo,
        } if fewer(&left) => Plan::Join {
            left: Box::new(place(*left, right, on, kind, rows)),
            right: other,
            kind: k,
            on: cond,
            algo,
        },
        input => semi(input, right, on, kind),
    }
}

fn semi(input: Plan, right: Plan, on: Expr, kind: JoinType) -> Plan {
    Plan::Join {
        left: Box::new(input),
        right: Box::new(right),
        kind,
        on: Some(on),
        algo: JoinAlgo::NestedLoop,
    }
}

/// The deepest reference of `e` to rows above its own (0: none).
fn depth(e: &Expr) -> usize {
    let mut max = 0;
    e.walk(&mut |x| match x {
        Expr::Outer { depth, .. } => max = max.max(*depth),
        Expr::Subquery(s) => max = max.max(s.plan.outer_depth().saturating_sub(1)),
        _ => {}
    });
    max
}

/// The subquery `s` as the right side of a join with rows `lw` wide, and
/// the join condition over the joined row.
fn right_side(s: &SubPlan, lw: usize) -> Option<(Plan, Expr)> {
    let operand = match &s.kind {
        SubKind::In(e, false) => Some(&**e),
        SubKind::Exists => None,
        _ => return None,
    };
    if !s.correlated || s.plan.outer_depth() == 0 {
        // Its rows are the same for every row: `IN` joins them as they are.
        let e = operand?;
        if s.plan.width() != 1 {
            return None;
        }
        let on = Expr::Binary(
            BinaryOp::Eq,
            Box::new(e.clone()),
            Box::new(Expr::Column(lw)),
        );
        return Some((s.plan.clone(), on));
    }
    if s.plan.outer_depth() > 1 {
        return None;
    }
    // Under what cannot change whether a row exists: one table's rows.
    let mut body = &s.plan;
    let mut value = None;
    loop {
        match body {
            Plan::Project { input, exprs } => {
                if value.is_none() {
                    value = exprs.first().cloned();
                }
                body = input;
            }
            Plan::Distinct { input } | Plan::Sort { input, .. } => body = input,
            _ => break,
        }
    }
    let (table, mut parts) = match body {
        Plan::Filter { input, predicate } => match &**input {
            Plan::Scan { table, filter, .. } => {
                let mut parts = Vec::new();
                conjuncts(predicate.clone(), &mut parts);
                if let Some(f) = filter {
                    conjuncts(f.clone(), &mut parts);
                }
                (table, parts)
            }
            _ => return None,
        },
        Plan::Scan { table, filter, .. } => {
            let mut parts = Vec::new();
            if let Some(f) = filter {
                conjuncts(f.clone(), &mut parts);
            }
            (table, parts)
        }
        _ => return None,
    };
    let value = match operand {
        Some(_) => {
            let v = value.unwrap_or(Expr::Column(0));
            if depth(&v) > 0 || v.has_subquery() {
                return None;
            }
            Some(v)
        }
        None => None,
    };
    // The conjuncts on the row: plain comparisons of it and the table's.
    let (corr, local): (Vec<Expr>, Vec<Expr>) = std::mem::take(&mut parts)
        .into_iter()
        .partition(|p| depth(p) > 0);
    if corr.is_empty() || corr.iter().any(|p| p.has_subquery() || depth(p) != 1) {
        return None;
    }
    if local.iter().any(|p| depth(p) > 0) {
        return None;
    }
    // Only the columns the condition reads.
    let mut cols: Vec<usize> = corr
        .iter()
        .chain(value.iter())
        .flat_map(Expr::columns)
        .collect();
    cols.sort_unstable();
    cols.dedup();
    if cols.is_empty() {
        return None;
    }
    let at = |c: usize| cols.iter().position(|&x| x == c).expect("a read column");
    let joined = |e: &Expr| {
        let mut e = e.clone();
        e.walk_mut(&mut |x| match x {
            Expr::Column(c) => *x = Expr::Column(lw + at(*c)),
            Expr::Outer { depth: 1, index } => *x = Expr::Column(*index),
            _ => {}
        });
        e
    };
    let mut on: Vec<Expr> = corr.iter().map(joined).collect();
    if let (Some(e), Some(v)) = (operand, &value) {
        on.insert(
            0,
            Expr::Binary(BinaryOp::Eq, Box::new(e.clone()), Box::new(joined(v))),
        );
    }
    let right = Plan::Project {
        input: Box::new(Plan::Scan {
            table: table.clone(),
            access: Access::Full,
            filter: conjunction(local),
            estimate: None,
        }),
        exprs: cols.iter().map(|&c| Expr::Column(c)).collect(),
    };
    Some((right, conjunction(on)?))
}
