//! The optimizer: rewrites a bound plan into a cheaper one with the same
//! result.
//!
//! - **Predicate pushdown**: filter conjuncts move as close to the scans as
//!   their columns allow — through projections, into join sides (respecting
//!   outer-join semantics) and into join conditions.
//! - **Join algorithm**: a join whose condition has `left = right`
//!   conjuncts becomes a hash join on them (the right input is the build
//!   side).
//! - **Access paths**: a scan whose filter pins a prefix of the primary key
//!   or of an index (`=` on leading columns, then a range on the next) can
//!   read only that key range.  The filter is kept, so the access path only
//!   has to include the matching rows.
//! - **Costs** (§M13): for tables with statistics (`ANALYZE`), the access
//!   path is the cheapest of a full scan and every usable index, and chains
//!   of inner joins are reordered by dynamic programming over their inputs
//!   (up to 8), avoiding cross products, with the smaller input as the hash
//!   build side.  Without statistics, the rule-based choice and the written
//!   join order are kept.

use std::ops::Bound;

use nucleus_kv::KvTxn;

use crate::ast::BinaryOp;
use crate::binder::{conjunction, conjuncts};
use crate::catalog::Table;
use crate::expr::Expr;
use crate::plan::{Access, JoinAlgo, JoinType, Plan};
use crate::stats::Stats;

/// Optimizes a plan; statistics are read through `txn`.
pub fn optimize(plan: Plan, txn: &dyn KvTxn) -> Plan {
    let stats = Stats::new(txn);
    let plan = push(plan, &stats);
    reorder(plan, &stats)
}

fn push(plan: Plan, st: &Stats) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => {
            let mut preds = Vec::new();
            conjuncts(predicate, &mut preds);
            push_into(*input, preds, st)
        }
        Plan::Scan { .. } | Plan::Join { .. } => push_into(plan, vec![], st),
        Plan::Project { input, exprs } => Plan::Project {
            input: Box::new(push(*input, st)),
            exprs,
        },
        Plan::Aggregate { input, group, aggs } => Plan::Aggregate {
            input: Box::new(push(*input, st)),
            group,
            aggs,
        },
        Plan::Sort { input, keys } => Plan::Sort {
            input: Box::new(push(*input, st)),
            keys,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: Box::new(push(*input, st)),
            limit,
            offset,
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(push(*input, st)),
        },
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => Plan::SetOp {
            op,
            left: Box::new(push(*left, st)),
            right: Box::new(push(*right, st)),
            all,
        },
        p @ (Plan::Values { .. }
        | Plan::Cte { .. }
        | Plan::Recursive { .. }
        | Plan::WorkTable { .. }
        | Plan::Function { .. }
        | Plan::Virtual { .. }) => p,
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

fn shift(e: &Expr, by: isize) -> Expr {
    e.map_columns(&|i| Expr::Column((i as isize + by) as usize))
}

/// Pushes `preds` (over `plan`'s output) as far down `plan` as they go.
fn push_into(plan: Plan, mut preds: Vec<Expr>, st: &Stats) -> Plan {
    match plan {
        Plan::Scan { table, filter, .. } => {
            if let Some(f) = filter {
                conjuncts(f, &mut preds);
            }
            let access = choose_access(&table, &preds, Some(st));
            let estimate = st.of(&table).map(|_| {
                let sel = conjunction(preds.clone()).map_or(1.0, |p| st.selectivity(&table, &p));
                (st.rows(&table) * sel).max(1.0)
            });
            Plan::Scan {
                table,
                access,
                filter: conjunction(preds),
                estimate,
            }
        }
        Plan::Filter { input, predicate } => {
            conjuncts(predicate, &mut preds);
            push_into(*input, preds, st)
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
                    input: Box::new(push_into(*input, through, st)),
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
                    // Both sides of a full join are preserved: nothing
                    // moves below it.
                    (_, JoinType::Full) => above.push(p),
                    ((true, _), _) => lp.push(p),
                    ((_, true), JoinType::Inner) => rp.push(shift(&p, -(lw as isize))),
                    (_, JoinType::Inner) => onp.push(p),
                    (_, JoinType::Left) => above.push(p),
                }
            }
            if let Some(on) = on {
                let mut parts = Vec::new();
                conjuncts(on, &mut parts);
                for p in parts {
                    match (side(&p), kind) {
                        (_, JoinType::Full) => onp.push(p),
                        // Only inner joins may move ON conjuncts on the
                        // preserved side out of the condition.
                        ((true, false), JoinType::Inner) => lp.push(p),
                        ((false, true), _) => rp.push(shift(&p, -(lw as isize))),
                        _ => onp.push(p),
                    }
                }
            }
            let left = push_into(*left, lp, st);
            let right = push_into(*right, rp, st);
            let algo = hash_keys(&onp, lw);
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
        other => filtered(push(other, st), preds),
    }
}

/// `left = right` conjuncts of a join condition as hash join keys.
fn hash_keys(onp: &[Expr], lw: usize) -> JoinAlgo {
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    for p in onp {
        if let Expr::Binary(BinaryOp::Eq, a, b) = p {
            if a.has_subquery() || b.has_subquery() {
                continue;
            }
            let (ac, bc) = (a.columns(), b.columns());
            let all_left = |c: &[usize]| !c.is_empty() && c.iter().all(|&x| x < lw);
            let all_right = |c: &[usize]| !c.is_empty() && c.iter().all(|&x| x >= lw);
            if all_left(&ac) && all_right(&bc) {
                left_keys.push((**a).clone());
                right_keys.push(shift(b, -(lw as isize)));
            } else if all_left(&bc) && all_right(&ac) {
                left_keys.push((**b).clone());
                right_keys.push(shift(a, -(lw as isize)));
            }
        }
    }
    if left_keys.is_empty() {
        JoinAlgo::NestedLoop
    } else {
        JoinAlgo::Hash {
            left_keys,
            right_keys,
        }
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

/// The access path for a scan with filter conjuncts `preds`.  With
/// statistics: the cheapest of a full scan and each usable index.
/// Without: the longest equality prefix, then a range on the next column,
/// the primary key winning ties.
pub fn choose_access(table: &Table, preds: &[Expr], st: Option<&Stats>) -> Access {
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
    let analyzed = st.filter(|s| s.of(table).is_some());
    let rows = analyzed.map_or(0.0, |s| s.rows(table));
    // (score or negative cost, access)
    let mut best: Option<(f64, Access)> = None;
    for (index, cols) in candidates {
        if cols.is_empty() {
            continue; // a hidden row id
        }
        let prefix: Vec<Expr> = cols.iter().map_while(|&c| eq(c)).collect();
        let (mut lower, mut upper) = (Bound::Unbounded, Bound::Unbounded);
        let mut used: Vec<Expr> = prefix
            .iter()
            .zip(&cols)
            .map(|(e, &c)| bin(BinaryOp::Eq, c, e))
            .collect();
        if let Some(&next) = cols.get(prefix.len()) {
            for (col, op, e) in &bounds {
                if *col != next {
                    continue;
                }
                let slot = match op {
                    BinaryOp::Gt | BinaryOp::GtEq => &mut lower,
                    BinaryOp::Lt | BinaryOp::LtEq => &mut upper,
                    _ => continue,
                };
                if *slot == Bound::Unbounded {
                    *slot = if matches!(op, BinaryOp::Gt | BinaryOp::Lt) {
                        Bound::Excluded(e.clone())
                    } else {
                        Bound::Included(e.clone())
                    };
                    used.push(bin(*op, next, e));
                }
            }
        }
        let range = usize::from(lower != Bound::Unbounded) + usize::from(upper != Bound::Unbounded);
        if prefix.is_empty() && range == 0 {
            continue;
        }
        let score = match analyzed {
            Some(s) => {
                let sel = conjunction(used).map_or(1.0, |p| s.selectivity(table, &p));
                // Seek once, then per row: 1 for the primary index, 3 for a
                // secondary one (its entry, then the row).
                let per_row = if index.is_none() { 1.0 } else { 3.0 };
                -(10.0 + rows * sel * per_row)
            }
            None => {
                let full = prefix.len() == cols.len();
                (prefix.len() * 4 + range + usize::from(full) * 2) as f64
            }
        };
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
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
    match (best, analyzed) {
        // A full scan beats an index that reads most of the table anyway.
        (Some((score, _)), Some(_)) if -score >= rows => Access::Full,
        (Some((_, a)), _) => a,
        (None, _) => Access::Full,
    }
}

fn bin(op: BinaryOp, c: usize, e: &Expr) -> Expr {
    Expr::Binary(op, Box::new(Expr::Column(c)), Box::new(e.clone()))
}

// ------------------------------------------------------------- join order

/// The table column a plan's output column comes from, if any.
fn origin(plan: &Plan, col: usize) -> Option<(&Table, usize)> {
    match plan {
        Plan::Scan { table, .. } => Some((table, col)),
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input } => origin(input, col),
        Plan::Project { input, exprs } => match exprs.get(col)? {
            Expr::Column(i) => origin(input, *i),
            _ => None,
        },
        Plan::Join { left, right, .. } => {
            let lw = left.width();
            if col < lw {
                origin(left, col)
            } else {
                origin(right, col - lw)
            }
        }
        _ => None,
    }
}

/// Estimated output rows of a plan.
fn estimate(plan: &Plan, st: &Stats) -> f64 {
    match plan {
        Plan::Scan {
            table,
            filter,
            estimate,
            ..
        } => estimate.unwrap_or_else(|| {
            st.rows(table) * filter.as_ref().map_or(1.0, |f| st.selectivity(table, f))
        }),
        Plan::Values { rows, .. } => rows.len() as f64,
        Plan::Cte { plan, .. } => estimate(plan, st),
        Plan::Recursive { base, .. } => estimate(base, st) * 10.0,
        Plan::WorkTable { .. } => 10.0,
        Plan::Function { .. } => 100.0,
        Plan::Virtual { .. } => 100.0,
        Plan::Filter { input, .. } => estimate(input, st) * 0.33,
        Plan::Project { input, .. } | Plan::Sort { input, .. } => estimate(input, st),
        Plan::Distinct { input } => estimate(input, st) * 0.5,
        Plan::Limit { input, limit, .. } => match limit {
            Some(Expr::Literal(crate::Value::Int(n))) => estimate(input, st).min(*n as f64),
            _ => estimate(input, st),
        },
        Plan::Aggregate { input, group, .. } => {
            if group.is_empty() {
                1.0
            } else {
                (estimate(input, st) * 0.1).max(1.0)
            }
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
            ..
        } => {
            let (l, r) = (estimate(left, st), estimate(right, st));
            let lw = left.width();
            let mut sel = 1.0;
            if let Some(on) = on {
                let mut parts = Vec::new();
                conjuncts(on.clone(), &mut parts);
                for p in &parts {
                    sel *= join_selectivity(
                        p,
                        |c| {
                            if c < lw {
                                origin(left, c)
                            } else {
                                origin(right, c - lw)
                            }
                        },
                        st,
                    );
                }
            }
            let out = l * r * sel;
            match kind {
                JoinType::Inner => out,
                JoinType::Left => out.max(l),
                JoinType::Full => out.max(l).max(r),
            }
        }
        Plan::SetOp {
            op, left, right, ..
        } => {
            let (l, r) = (estimate(left, st), estimate(right, st));
            match op {
                crate::ast::SetOp::Union => l + r,
                crate::ast::SetOp::Intersect => l.min(r),
                crate::ast::SetOp::Except => l,
            }
        }
    }
}

/// The selectivity of a join conjunct: `1 / max(distinct)` for an equality
/// of two columns, a third otherwise.
fn join_selectivity<'p>(
    p: &Expr,
    origin_of: impl Fn(usize) -> Option<(&'p Table, usize)>,
    st: &Stats,
) -> f64 {
    if let Expr::Binary(BinaryOp::Eq, a, b) = p
        && let (Expr::Column(x), Expr::Column(y)) = (&**a, &**b)
    {
        let d = |c: usize| origin_of(c).map_or(10.0, |(t, col)| st.distinct(t, col));
        return 1.0 / d(*x).max(d(*y)).max(1.0);
    }
    0.33
}

fn analyzed(plan: &Plan, st: &Stats) -> bool {
    match plan {
        Plan::Scan { table, .. } => st.of(table).is_some(),
        p => p.children().into_iter().any(|c| analyzed(c, st)),
    }
}

/// Reorders chains of inner joins (bottom-up through the plan).
fn reorder(plan: Plan, st: &Stats) -> Plan {
    if !matches!(
        plan,
        Plan::Join {
            kind: JoinType::Inner,
            ..
        }
    ) {
        return map_children(plan, &|c| reorder(c, st));
    }
    // The whole chain at once (top-down): reordering an inner part first
    // would wrap it in a projection and hide it from the chain.
    let mut leaves = Vec::new();
    let mut conds = Vec::new();
    collect(plan.clone(), 0, &mut leaves, &mut conds);
    let leaves: Vec<(Plan, usize)> = leaves
        .into_iter()
        .map(|(p, off)| (reorder(p, st), off))
        .collect();
    if leaves.len() > 8 || !leaves.iter().any(|(p, _)| analyzed(p, st)) {
        return map_children(plan, &|c| reorder(c, st));
    }
    let fallback = plan.clone();
    reorder_leaves(leaves, conds, st).unwrap_or_else(|| map_children(fallback, &|c| reorder(c, st)))
}

fn map_children(plan: Plan, f: &dyn Fn(Plan) -> Plan) -> Plan {
    let b = |p: Box<Plan>| Box::new(f(*p));
    match plan {
        Plan::Filter { input, predicate } => Plan::Filter {
            input: b(input),
            predicate,
        },
        Plan::Project { input, exprs } => Plan::Project {
            input: b(input),
            exprs,
        },
        Plan::Join {
            left,
            right,
            kind,
            on,
            algo,
        } => Plan::Join {
            left: b(left),
            right: b(right),
            kind,
            on,
            algo,
        },
        Plan::Aggregate { input, group, aggs } => Plan::Aggregate {
            input: b(input),
            group,
            aggs,
        },
        Plan::Sort { input, keys } => Plan::Sort {
            input: b(input),
            keys,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: b(input),
            limit,
            offset,
        },
        Plan::Distinct { input } => Plan::Distinct { input: b(input) },
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => Plan::SetOp {
            op,
            left: b(left),
            right: b(right),
            all,
        },
        p @ (Plan::Scan { .. }
        | Plan::Values { .. }
        | Plan::Cte { .. }
        | Plan::Recursive { .. }
        | Plan::WorkTable { .. }
        | Plan::Function { .. }
        | Plan::Virtual { .. }) => p,
    }
}

/// The inputs of a chain of inner joins (with their column offsets) and
/// its conditions, in the chain's combined column numbering.
fn collect(plan: Plan, offset: usize, leaves: &mut Vec<(Plan, usize)>, conds: &mut Vec<Expr>) {
    match plan {
        Plan::Join {
            left,
            right,
            kind: JoinType::Inner,
            on,
            ..
        } => {
            let lw = left.width();
            collect(*left, offset, leaves, conds);
            collect(*right, offset + lw, leaves, conds);
            if let Some(on) = on {
                let mut parts = Vec::new();
                conjuncts(on, &mut parts);
                conds.extend(parts.iter().map(|p| shift(p, offset as isize)));
            }
        }
        leaf => leaves.push((leaf, offset)),
    }
}

#[derive(Clone)]
enum Tree {
    Leaf(usize),
    Node(Box<Tree>, Box<Tree>),
}

fn reorder_leaves(leaves: Vec<(Plan, usize)>, conds: Vec<Expr>, st: &Stats) -> Option<Plan> {
    let n = leaves.len();
    let total: usize = leaves.iter().map(|(p, _)| p.width()).sum();
    let leaf_of = |col: usize| {
        leaves
            .iter()
            .rposition(|(_, off)| *off <= col)
            .expect("column inside the chain")
    };
    let cond_mask: Vec<usize> = conds
        .iter()
        .map(|c| {
            c.columns()
                .iter()
                .fold(0, |m, &col| m | (1 << leaf_of(col)))
        })
        .collect();
    let cond_sel: Vec<f64> = conds
        .iter()
        .map(|c| {
            join_selectivity(
                c,
                |col| {
                    let l = leaf_of(col);
                    origin(&leaves[l].0, col - leaves[l].1)
                },
                st,
            )
        })
        .collect();
    let leaf_rows: Vec<f64> = leaves
        .iter()
        .map(|(p, _)| estimate(p, st).max(1.0))
        .collect();
    let full = (1usize << n) - 1;
    let rows_of = |mask: usize| -> f64 {
        let mut r: f64 = (0..n)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| leaf_rows[i])
            .product();
        for (m, s) in cond_mask.iter().zip(&cond_sel) {
            if *m & mask == *m && m.count_ones() >= 2 {
                r *= s;
            }
        }
        r.max(1.0)
    };
    let connected = |a: usize, b: usize| {
        cond_mask
            .iter()
            .any(|m| m & a != 0 && m & b != 0 && m & !(a | b) == 0)
    };
    let mut best: Vec<Option<(f64, Tree)>> = vec![None; full + 1];
    for i in 0..n {
        best[1 << i] = Some((leaf_rows[i], Tree::Leaf(i)));
    }
    let mut masks: Vec<usize> = (1..=full).filter(|m| m.count_ones() >= 2).collect();
    masks.sort_by_key(|m| m.count_ones());
    for mask in masks {
        let out = rows_of(mask);
        for allow_cross in [false, true] {
            let mut a = (mask - 1) & mask;
            while a > 0 {
                let b = mask & !a;
                if let (Some((ca, ta)), Some((cb, tb))) = (&best[a], &best[b]) {
                    let joined = connected(a, b);
                    if joined || allow_cross {
                        let (ra, rb) = (rows_of(a), rows_of(b));
                        let step = if joined { ra + 1.5 * rb + out } else { ra * rb };
                        let cost = ca + cb + step;
                        if best[mask].as_ref().is_none_or(|(c, _)| cost < *c) {
                            best[mask] = Some((
                                cost,
                                Tree::Node(Box::new(ta.clone()), Box::new(tb.clone())),
                            ));
                        }
                    }
                }
                a = (a - 1) & mask;
            }
            if best[mask].is_some() {
                break;
            }
        }
    }
    let (_, tree) = best[full].clone()?;
    let mut assigned = vec![false; conds.len()];
    let leaves: Vec<(Plan, usize, usize)> = leaves
        .into_iter()
        .map(|(p, off)| {
            let w = p.width();
            (p, off, w)
        })
        .collect();
    let (plan, layout) = build(&tree, &leaves, &conds, &cond_mask, &mut assigned, st);
    // Conditions on no leaf (constants) at the top.
    let rest: Vec<Expr> = conds
        .iter()
        .zip(&assigned)
        .filter(|(_, a)| !**a)
        .map(|(c, _)| c.clone())
        .collect();
    let pos = |g: usize| {
        layout
            .iter()
            .position(|&x| x == g)
            .expect("every column placed")
    };
    let plan = filtered(
        plan,
        rest.iter()
            .map(|c| c.map_columns(&|g| Expr::Column(pos(g))))
            .collect(),
    );
    Some(Plan::Project {
        input: Box::new(plan),
        exprs: (0..total).map(|g| Expr::Column(pos(g))).collect(),
    })
}

fn tree_mask(t: &Tree) -> usize {
    match t {
        Tree::Leaf(i) => 1 << i,
        Tree::Node(a, b) => tree_mask(a) | tree_mask(b),
    }
}

fn build(
    t: &Tree,
    leaves: &[(Plan, usize, usize)],
    conds: &[Expr],
    masks: &[usize],
    assigned: &mut [bool],
    st: &Stats,
) -> (Plan, Vec<usize>) {
    match t {
        Tree::Leaf(i) => {
            let (p, off, w) = &leaves[*i];
            (p.clone(), (*off..off + w).collect())
        }
        Tree::Node(a, b) => {
            let (pa, la) = build(a, leaves, conds, masks, assigned, st);
            let (pb, lb) = build(b, leaves, conds, masks, assigned, st);
            let (ma, mb) = (tree_mask(a), tree_mask(b));
            let mut layout = la;
            layout.extend(lb);
            let pos = |g: usize| {
                layout
                    .iter()
                    .position(|&x| x == g)
                    .expect("column in this join")
            };
            let mut on = Vec::new();
            for (i, c) in conds.iter().enumerate() {
                let m = masks[i];
                if !assigned[i] && m != 0 && m & !(ma | mb) == 0 {
                    assigned[i] = true;
                    on.push(c.map_columns(&|g| Expr::Column(pos(g))));
                }
            }
            let join = Plan::Join {
                left: Box::new(pa),
                right: Box::new(pb),
                kind: JoinType::Inner,
                on: conjunction(on),
                algo: JoinAlgo::NestedLoop,
            };
            (push_into(join, vec![], st), layout)
        }
    }
}
