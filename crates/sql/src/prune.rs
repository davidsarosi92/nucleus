//! Which columns each table scan of a plan needs (PLAN.md §M14.11 L7): the
//! plan is walked from the top with the output columns wanted, each node
//! adding what its expressions read; a scan decodes only its needed
//! columns, the rest reading as NULL (the row keeps its width, so no
//! position changes).  A subquery's plan is walked too, from all of its own
//! output.  Conservative: an expression with a subquery needs
//! every column (a correlated subquery reads the row it runs for), and so
//! does anything that compares whole rows (`DISTINCT`, set operations) or
//! is shared (CTEs, recursion).

use std::collections::HashMap;

use crate::expr::Expr;
use crate::plan::{JoinAlgo, Plan};

/// The needed columns of each scan in `plan`, by the scan's address — only
/// scans that need fewer than all their columns.
pub fn needed(plan: &Plan) -> HashMap<usize, Vec<bool>> {
    let mut out = HashMap::new();
    walk(plan, vec![true; plan.width()], &mut out);
    out
}

/// Marks in `into` (positions shifted by `offset`) the columns `e` reads;
/// every one when it has a subquery.
fn read(e: &Expr, into: &mut [bool], offset: isize) {
    if e.has_subquery() {
        into.iter_mut().for_each(|n| *n = true);
        return;
    }
    for c in e.columns() {
        let at = c as isize + offset;
        if at >= 0 && (at as usize) < into.len() {
            into[at as usize] = true;
        }
    }
}

fn walk(plan: &Plan, mut wanted: Vec<bool>, out: &mut HashMap<usize, Vec<bool>>) {
    let all = |p: &Plan| vec![true; p.width()];
    // The plans of this node's subqueries: each all of its own output.
    plan.for_each_expr(&mut |e| {
        e.walk(&mut |x| {
            if let crate::expr::Expr::Subquery(s) = x {
                walk(&s.plan, vec![true; s.plan.width()], out);
            }
        })
    });
    match plan {
        Plan::Scan { filter, .. } => {
            if let Some(f) = filter {
                read(f, &mut wanted, 0);
            }
            if wanted.iter().any(|w| !w) {
                out.insert(plan as *const Plan as usize, wanted);
            }
        }
        Plan::Filter { input, predicate }
        | Plan::Secure {
            input,
            policy: predicate,
        } => {
            read(predicate, &mut wanted, 0);
            walk(input, wanted, out);
        }
        Plan::Project { input, exprs } => {
            let mut need = vec![false; input.width()];
            for (e, w) in exprs.iter().zip(&wanted) {
                if *w {
                    read(e, &mut need, 0);
                }
            }
            walk(input, need, out);
        }
        Plan::Join {
            left,
            right,
            on,
            algo,
            ..
        } => {
            let lw = left.width();
            // (A semi or anti join's condition reads the right row too.)
            wanted.resize(lw + right.width(), false);
            if let Some(on) = on {
                read(on, &mut wanted, 0);
            }
            let mut right_need = wanted.split_off(lw.min(wanted.len()));
            right_need.resize(right.width(), false);
            let mut left_need = wanted;
            left_need.resize(lw, false);
            if let JoinAlgo::Hash {
                left_keys,
                right_keys,
                ..
            } = algo
            {
                for k in left_keys {
                    read(k, &mut left_need, 0);
                }
                for k in right_keys {
                    read(k, &mut right_need, 0);
                }
            }
            walk(left, left_need, out);
            walk(right, right_need, out);
        }
        Plan::Aggregate { input, group, aggs } => {
            let mut need = vec![false; input.width()];
            for g in group {
                read(g, &mut need, 0);
            }
            for a in aggs {
                for e in &a.args {
                    read(e, &mut need, 0);
                }
                for k in &a.order {
                    read(&k.expr, &mut need, 0);
                }
            }
            walk(input, need, out);
        }
        Plan::Sort { input, keys } => {
            for k in keys {
                read(&k.expr, &mut wanted, 0);
            }
            walk(input, wanted, out);
        }
        Plan::Limit { input, .. } => walk(input, wanted, out),
        Plan::Distinct { input } => walk(input, all(input), out),
        Plan::SetOp { left, right, .. } => {
            walk(left, all(left), out);
            walk(right, all(right), out);
        }
        Plan::Append { inputs, .. } => {
            for p in inputs {
                walk(p, wanted.clone(), out);
            }
        }
        Plan::Cte { plan: inner, .. } => walk(inner, all(inner), out),
        Plan::Recursive { base, step, .. } => {
            walk(base, all(base), out);
            walk(step, all(step), out);
        }
        Plan::Values { .. }
        | Plan::Function { .. }
        | Plan::View { .. }
        | Plan::WorkTable { .. } => {}
    }
}
