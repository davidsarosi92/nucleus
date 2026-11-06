//! Eager aggregation (PLAN.md §M14.20 V13; Yan & Larson, VLDB 1995): a
//! grouping by the left side of a join, whose left rows are each a group
//! of their own, computed on the right side by its join key first — then
//! joined, one right row a left row at most.
//!
//! `SELECT c.id, count(o.id) FROM c LEFT JOIN o ON c.id = o.cid GROUP BY
//! c.id` becomes `c LEFT JOIN (SELECT cid, count(id) FROM o GROUP BY cid)`
//! — a group a customer rather than a row an order through the join
//! (TPC-H Q13).  It holds when:
//!
//! - the join is inner or left, hashed on equalities and nothing else;
//! - the grouping is by left columns among which are the left keys, and
//!   those are a key of the left side's single table (each left row its
//!   own group, met by its right rows once);
//! - every aggregate reads right columns only and cannot fail
//!   ([`crate::ext::Aggregate::infallible`]) — the right rows no left row
//!   matches are aggregated too, so nothing they hold may raise an error
//!   the join would have kept from coming;
//! - the right side has at most half as many groups as rows (else the
//!   grouping saves the join nothing), and the join keeps at least half of
//!   its rows (else the join narrows it more cheaply — its key filter,
//!   [`crate::keyfilter`] — than a grouping of all of them would).
//!
//! An inner join is tried both ways round (grouped by its right side's key,
//! its left side is aggregated), and through a projection of columns.
//!
//! A left row no right row matches (a left join) gets what its aggregate
//! gives for the padded row, a constant: `count(x)` 0, `count(*)` 1,
//! `min(x)` `NULL`.

use crate::binder::conjuncts;
use crate::expr::Expr;
use crate::plan::{Agg, JoinAlgo, JoinType, Plan};
use crate::stats::Stats;
use crate::types::Value;

/// The plan with its eligible groupings over joins aggregated early.
pub(crate) fn eager(plan: Plan, st: &Stats) -> Plan {
    let plan = crate::optimizer::map_children(plan, &|c| eager(c, st));
    let Plan::Aggregate { input, group, aggs } = plan else {
        return plan;
    };
    match rewrite(&input, &group, &aggs, st) {
        Some(p) => p,
        None => Plan::Aggregate { input, group, aggs },
    }
}

fn rewrite(input: &Plan, group: &[Expr], aggs: &[Agg], st: &Stats) -> Option<Plan> {
    // Through a projection of columns: of the columns below.
    if let Plan::Project { input, exprs } = input
        && exprs.iter().all(|e| matches!(e, Expr::Column(_)))
    {
        let below = |c: usize| exprs[c].clone();
        let group: Vec<Expr> = group.iter().map(|g| g.map_columns(&below)).collect();
        let aggs: Vec<Agg> = aggs.iter().map(|a| map_agg(a, &below)).collect();
        return rewrite(input, &group, &aggs, st);
    }
    if let Some(p) = rewrite_join(input, group, aggs, st) {
        return Some(p);
    }
    // An inner join the other way round.
    let Plan::Join {
        left,
        right,
        kind: JoinType::Inner,
        on,
        algo:
            JoinAlgo::Hash {
                left_keys,
                right_keys,
                build_left: false,
                ..
            },
    } = input
    else {
        return None;
    };
    let (lw, rw) = (left.width(), right.width());
    let flip = |c: usize| Expr::Column(if c < lw { c + rw } else { c - lw });
    let swapped = Plan::Join {
        left: right.clone(),
        right: left.clone(),
        kind: JoinType::Inner,
        on: on.as_ref().map(|e| e.map_columns(&flip)),
        algo: JoinAlgo::Hash {
            left_keys: right_keys.clone(),
            right_keys: left_keys.clone(),
            build_left: false,
            adaptive: None,
        },
    };
    let group: Vec<Expr> = group.iter().map(|g| g.map_columns(&flip)).collect();
    let aggs: Vec<Agg> = aggs.iter().map(|a| map_agg(a, &flip)).collect();
    rewrite_join(&swapped, &group, &aggs, st)
}

/// `a` with its columns mapped by `f`.
fn map_agg(a: &Agg, f: &dyn Fn(usize) -> Expr) -> Agg {
    Agg {
        func: a.func.clone(),
        args: a.args.iter().map(|e| e.map_columns(f)).collect(),
        distinct: a.distinct,
        order: a
            .order
            .iter()
            .map(|k| crate::plan::SortKey {
                expr: k.expr.map_columns(f),
                desc: k.desc,
                nulls_first: k.nulls_first,
            })
            .collect(),
    }
}

fn rewrite_join(input: &Plan, group: &[Expr], aggs: &[Agg], st: &Stats) -> Option<Plan> {
    let Plan::Join {
        left,
        right,
        kind: kind @ (JoinType::Inner | JoinType::Left),
        on,
        algo:
            JoinAlgo::Hash {
                left_keys,
                right_keys,
                build_left: false,
                ..
            },
    } = input
    else {
        return None;
    };
    let lw = left.width();
    // The condition: the keys' equalities only.
    let mut parts = Vec::new();
    if let Some(on) = on {
        conjuncts(on.clone(), &mut parts);
    }
    let pairs: Vec<(Expr, Expr)> = left_keys
        .iter()
        .zip(right_keys)
        .map(|(l, r)| (l.clone(), r.map_columns(&|c| Expr::Column(c + lw))))
        .collect();
    let is_pair = |p: &Expr| match p {
        Expr::Binary(crate::ast::BinaryOp::Eq, a, b) => pairs
            .iter()
            .any(|(l, r)| (**a == *l && **b == *r) || (**a == *r && **b == *l)),
        _ => false,
    };
    if !parts.iter().all(is_pair) {
        return None;
    }
    // Grouped by left columns that hold the left keys, a key of its table.
    let group_cols: Vec<usize> = group
        .iter()
        .map(|g| match g {
            Expr::Column(c) if *c < lw => Some(*c),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let key_cols: Vec<usize> = left_keys
        .iter()
        .map(|k| match k {
            Expr::Column(c) => Some(*c),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if !key_cols.iter().all(|k| group_cols.contains(k)) || !unique(left, &key_cols) {
        return None;
    }
    // Aggregates of right columns that cannot fail; each one's value for
    // a padded row.
    let right_only = |e: &Expr| e.columns().iter().all(|&c| c >= lw) && !e.has_subquery();
    let mut padded = Vec::with_capacity(aggs.len());
    for a in aggs {
        if !a.func.infallible()
            || !a
                .args
                .iter()
                .all(|e| matches!(e, Expr::Column(c) if *c >= lw))
            || !a.order.iter().all(|k| right_only(&k.expr))
        {
            return None;
        }
        padded.push(padded_value(a)?);
    }
    // Worth it: the right side's groups at most half its rows, and the
    // join keeping at least half of them.
    let rows = crate::optimizer::estimate(right, st);
    let groups = crate::optimizer::groups_of_exprs(right, right_keys, st)?;
    if groups > rows / 2.0 || crate::optimizer::estimate(input, st) < rows / 2.0 {
        return None;
    }
    // The right side grouped by its keys: the keys, then the aggregates.
    let nk = right_keys.len();
    let shifted: Vec<Agg> = aggs
        .iter()
        .map(|a| map_agg(a, &|c| Expr::Column(c - lw)))
        .collect();
    let grouped = Plan::Aggregate {
        input: right.clone(),
        group: right_keys.clone(),
        aggs: shifted,
    };
    let on = left_keys
        .iter()
        .enumerate()
        .map(|(j, l)| {
            Expr::Binary(
                crate::ast::BinaryOp::Eq,
                Box::new(l.clone()),
                Box::new(Expr::Column(lw + j)),
            )
        })
        .reduce(|a, b| Expr::Binary(crate::ast::BinaryOp::And, Box::new(a), Box::new(b)));
    let join = Plan::Join {
        left: left.clone(),
        right: Box::new(grouped),
        kind: *kind,
        on,
        algo: JoinAlgo::Hash {
            left_keys: left_keys.clone(),
            right_keys: (0..nk).map(Expr::Column).collect(),
            build_left: false,
            adaptive: None,
        },
    };
    // The grouping's columns: the group's, then each aggregate's (a padded
    // row's value where no right row matched: its key is NULL then).
    let mut exprs: Vec<Expr> = group_cols.iter().map(|&c| Expr::Column(c)).collect();
    for (i, v) in padded.into_iter().enumerate() {
        let value = Expr::Column(lw + nk + i);
        exprs.push(if *kind == JoinType::Inner || v.is_null() {
            value
        } else {
            Expr::Case {
                operand: None,
                whens: vec![(
                    Expr::IsNull(Box::new(Expr::Column(lw)), false),
                    Expr::Literal(v),
                )],
                otherwise: Some(Box::new(value)),
            }
        });
    }
    Some(Plan::Project {
        input: Box::new(join),
        exprs,
    })
}

/// What aggregate `a` gives over a row its join padded: its arguments
/// (right columns) all `NULL`.
fn padded_value(a: &Agg) -> Option<Value> {
    let mut acc = a.func.start();
    if a.args.is_empty() || a.func.keeps_nulls() {
        acc.add(&vec![Value::Null; a.args.len()]).ok()?;
    }
    acc.finish().ok()
}

/// Whether `cols` of `plan`'s rows are a key: `plan` reads one table
/// (through filters, projections, sorts), and the columns are, or hold,
/// its primary key (not a unique index: its `NULL`s repeat, and group
/// together).
fn unique(plan: &Plan, cols: &[usize]) -> bool {
    fn table_cols<'p>(
        plan: &'p Plan,
        cols: &[usize],
    ) -> Option<(&'p crate::catalog::Table, Vec<usize>)> {
        match plan {
            Plan::Scan { table, .. } => Some((table, cols.to_vec())),
            Plan::Filter { input, .. } | Plan::Sort { input, .. } | Plan::Distinct { input } => {
                table_cols(input, cols)
            }
            Plan::Project { input, exprs } => {
                let mapped = cols
                    .iter()
                    .map(|&c| match exprs.get(c) {
                        Some(Expr::Column(i)) => Some(*i),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                table_cols(input, &mapped)
            }
            _ => None,
        }
    }
    let Some((table, cols)) = table_cols(plan, cols) else {
        return false;
    };
    !table.primary_key.is_empty() && table.primary_key.iter().all(|k| cols.contains(k))
}
