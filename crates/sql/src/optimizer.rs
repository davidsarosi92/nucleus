//! The optimizer: rewrites a bound plan into a cheaper one with the same
//! result.
//!
//! - **Predicate pushdown**: filter conjuncts move as close to the scans as
//!   their columns allow — through projections, into join sides (respecting
//!   outer-join semantics) and into join conditions.
//! - **Join algorithm**: a join whose condition has `left = right`
//!   conjuncts becomes a hash join on them.
//! - **Access paths**: a scan whose filter pins a prefix of the primary key
//!   or of an index (`=` on leading columns, then a range on the next) reads
//!   only that key range.  The filter is kept, so the access path only has
//!   to include the matching rows.
//!
//! Costs (statistics, join order) come with §M13.

use std::ops::Bound;

use crate::ast::BinaryOp;
use crate::binder::{conjunction, conjuncts};
use crate::catalog::Table;
use crate::expr::Expr;
use crate::plan::{Access, JoinAlgo, JoinType, Plan};

/// Optimizes a plan.
pub fn optimize(plan: Plan) -> Plan {
    push(plan)
}

fn push(plan: Plan) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => {
            let mut preds = Vec::new();
            conjuncts(predicate, &mut preds);
            push_into(*input, preds)
        }
        Plan::Scan { .. } => push_into(plan, vec![]),
        Plan::Join { .. } => push_into(plan, vec![]),
        Plan::Project { input, exprs } => Plan::Project {
            input: Box::new(push(*input)),
            exprs,
        },
        Plan::Aggregate { input, group, aggs } => Plan::Aggregate {
            input: Box::new(push(*input)),
            group,
            aggs,
        },
        Plan::Sort { input, keys } => Plan::Sort {
            input: Box::new(push(*input)),
            keys,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: Box::new(push(*input)),
            limit,
            offset,
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(push(*input)),
        },
        Plan::Union { left, right, all } => Plan::Union {
            left: Box::new(push(*left)),
            right: Box::new(push(*right)),
            all,
        },
        p @ Plan::Values { .. } => p,
    }
}

fn filtered(plan: Plan, preds: Vec<Expr>) -> Plan {
    match conjunction(preds) {
        None => plan,
        Some(predicate) => Plan::Filter {
            input: Box::new(plan),
            predicate,
        },
    }
}

fn shift(e: &Expr, by: usize) -> Expr {
    e.map_columns(&|i| Expr::Column(i - by))
}

/// Pushes `preds` (over `plan`'s output) as far down `plan` as they go.
fn push_into(plan: Plan, mut preds: Vec<Expr>) -> Plan {
    match plan {
        Plan::Scan {
            table,
            filter,
            estimate,
            ..
        } => {
            if let Some(f) = filter {
                conjuncts(f, &mut preds);
            }
            let access = choose_access(&table, &preds);
            Plan::Scan {
                table,
                access,
                filter: conjunction(preds),
                estimate,
            }
        }
        Plan::Filter { input, predicate } => {
            conjuncts(predicate, &mut preds);
            push_into(*input, preds)
        }
        Plan::Project { input, exprs } => {
            // Through the projection, by substituting its expressions —
            // unless they hold subqueries (they would run more often).
            let (through, stay): (Vec<Expr>, Vec<Expr>) = preds
                .into_iter()
                .partition(|p| p.columns().iter().all(|&c| !exprs[c].has_subquery()));
            let through: Vec<Expr> = through
                .iter()
                .map(|p| p.map_columns(&|i| exprs[i].clone()))
                .collect();
            filtered(
                Plan::Project {
                    input: Box::new(push_into(*input, through)),
                    exprs,
                },
                stay,
            )
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
            ..
        } => {
            let lw = left.width();
            let (mut lp, mut rp, mut onp, mut above) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let side = |e: &Expr| -> (bool, bool) {
                let cols = e.columns();
                (cols.iter().all(|&c| c < lw), cols.iter().all(|&c| c >= lw))
            };
            for p in preds {
                match (side(&p), kind) {
                    ((true, _), _) => lp.push(p),
                    ((_, true), JoinType::Inner) => rp.push(shift(&p, lw)),
                    (_, JoinType::Inner) => onp.push(p),
                    (_, JoinType::Left) => above.push(p),
                }
            }
            if let Some(on) = on {
                let mut parts = Vec::new();
                conjuncts(on, &mut parts);
                for p in parts {
                    match (side(&p), kind) {
                        // Only inner joins may move ON conjuncts on the
                        // preserved side out of the condition.
                        ((true, false), JoinType::Inner) => lp.push(p),
                        ((false, true), _) => rp.push(shift(&p, lw)),
                        _ => onp.push(p),
                    }
                }
            }
            let left = push_into(*left, lp);
            let right = push_into(*right, rp);
            // Equi-join keys.
            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            for p in &onp {
                if let Expr::Binary(BinaryOp::Eq, a, b) = p {
                    if a.has_subquery() || b.has_subquery() {
                        continue;
                    }
                    let (ac, bc) = (a.columns(), b.columns());
                    let all_left = |c: &[usize]| !c.is_empty() && c.iter().all(|&x| x < lw);
                    let all_right = |c: &[usize]| !c.is_empty() && c.iter().all(|&x| x >= lw);
                    if all_left(&ac) && all_right(&bc) {
                        left_keys.push((**a).clone());
                        right_keys.push(shift(b, lw));
                    } else if all_left(&bc) && all_right(&ac) {
                        left_keys.push((**b).clone());
                        right_keys.push(shift(a, lw));
                    }
                }
            }
            let algo = if left_keys.is_empty() {
                JoinAlgo::NestedLoop
            } else {
                JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                }
            };
            filtered(
                Plan::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind,
                    on: conjunction(onp),
                    algo,
                },
                above,
            )
        }
        other => filtered(push(other), preds),
    }
}

/// `Column(c) op e` with `e` free of this row's columns (and of
/// subqueries): `(c, op, e)` with `op` as if the column were on the left.
fn column_bound(p: &Expr) -> Option<(usize, BinaryOp, Expr)> {
    let Expr::Binary(op, a, b) = p else {
        return None;
    };
    let flip = |op: BinaryOp| match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        op => op,
    };
    let free = |e: &Expr| e.columns().is_empty() && !e.has_subquery();
    if !matches!(
        op,
        BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
    ) {
        return None;
    }
    match (&**a, &**b) {
        (Expr::Column(c), e) if free(e) => Some((*c, *op, e.clone())),
        (e, Expr::Column(c)) if free(e) => Some((*c, flip(*op), e.clone())),
        _ => None,
    }
}

/// The best index for a filter (rule-based until §M13): the longest
/// equality prefix, then a range on the next column; the primary key wins
/// ties.
pub fn choose_access(table: &Table, preds: &[Expr]) -> Access {
    let bounds: Vec<(usize, BinaryOp, Expr)> = preds.iter().filter_map(column_bound).collect();
    let eq = |c: usize| {
        bounds
            .iter()
            .find(|(col, op, _)| *col == c && *op == BinaryOp::Eq)
            .map(|(_, _, e)| e.clone())
    };
    let candidates = std::iter::once((None, table.primary_key.clone())).chain(
        table
            .indexes
            .iter()
            .enumerate()
            .map(|(i, ix)| (Some(i), ix.columns.clone())),
    );
    let mut best: Option<(usize, Access)> = None;
    for (index, cols) in candidates {
        if cols.is_empty() {
            continue; // a hidden row id
        }
        let prefix: Vec<Expr> = cols.iter().map_while(|&c| eq(c)).collect();
        let (mut lower, mut upper) = (Bound::Unbounded, Bound::Unbounded);
        if let Some(&next) = cols.get(prefix.len()) {
            for (col, op, e) in &bounds {
                if *col != next {
                    continue;
                }
                match op {
                    BinaryOp::Gt if lower == Bound::Unbounded => lower = Bound::Excluded(e.clone()),
                    BinaryOp::GtEq if lower == Bound::Unbounded => {
                        lower = Bound::Included(e.clone())
                    }
                    BinaryOp::Lt if upper == Bound::Unbounded => upper = Bound::Excluded(e.clone()),
                    BinaryOp::LtEq if upper == Bound::Unbounded => {
                        upper = Bound::Included(e.clone())
                    }
                    _ => {}
                }
            }
        }
        let range = usize::from(lower != Bound::Unbounded) + usize::from(upper != Bound::Unbounded);
        let full = prefix.len() == cols.len();
        let score = prefix.len() * 4 + range + usize::from(full) * 2;
        if score > 0 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((
                score,
                Access::Key {
                    index,
                    prefix,
                    lower,
                    upper,
                },
            ));
        }
    }
    best.map_or(Access::Full, |(_, a)| a)
}
