//! Narrow rows through joins (PLAN.md §M14.20 V10): a join copies each
//! row it gives — its left row and its right row, every column — so the
//! columns nothing above reads are dropped below it.  The plan is walked
//! from the top with the columns wanted; each node keeps what it and the
//! nodes above read, a join's inputs are cut to that by a projection, and
//! every reference above follows the columns to their new places (a
//! correlated subquery's to its row's too).  Where nothing narrows, the plan
//! is as it was.

use crate::expr::Expr;
use crate::plan::{JoinAlgo, JoinType, Plan};

/// `plan` with only the columns read above passing its joins; its output
/// is all of its columns, in order.
pub fn narrow(plan: Plan) -> Plan {
    let w = plan.width();
    let (plan, map) = walk(plan, &vec![true; w]);
    // Every column wanted: each at its place.
    debug_assert!((0..w).all(|c| map[c] == Some(c)));
    plan
}

/// Marks in `into` the columns `e` reads.
fn read(e: &Expr, into: &mut [bool]) {
    for c in e.reads() {
        if let Some(w) = into.get_mut(c) {
            *w = true;
        }
    }
}

fn identity(w: usize) -> Vec<Option<usize>> {
    (0..w).map(Some).collect()
}

/// An expression's columns moved by `map`.
fn moved(e: &Expr, map: &[Option<usize>]) -> Expr {
    e.map_columns(&|c| Expr::Column(map[c].expect("a column read is wanted")))
}

/// `plan` keeping at least the `wanted` columns, and where each old column
/// is now (`None`: dropped).
fn walk(plan: Plan, wanted: &[bool]) -> (Plan, Vec<Option<usize>>) {
    let w = plan.width();
    match plan {
        Plan::Project { input, exprs } => {
            // Only the wanted outputs (the first, if none is: a row stays a
            // row).
            let keep: Vec<usize> = (0..w).filter(|&c| wanted[c]).collect();
            let keep = if keep.is_empty() && w > 0 {
                vec![0]
            } else {
                keep
            };
            let mut need = vec![false; input.width()];
            for &c in &keep {
                read(&exprs[c], &mut need);
            }
            let (input, imap) = walk(*input, &need);
            let mut map = vec![None; w];
            let mut out = Vec::with_capacity(keep.len());
            for (at, &c) in keep.iter().enumerate() {
                map[c] = Some(at);
                out.push(moved(&exprs[c], &imap));
            }
            (
                Plan::Project {
                    input: Box::new(input),
                    exprs: out,
                },
                map,
            )
        }
        Plan::Filter { input, predicate } => {
            let mut need = wanted.to_vec();
            read(&predicate, &mut need);
            let (input, map) = walk(*input, &need);
            let predicate = moved(&predicate, &map);
            (
                Plan::Filter {
                    input: Box::new(input),
                    predicate,
                },
                map,
            )
        }
        Plan::Sort { input, keys } => {
            let mut need = wanted.to_vec();
            for k in &keys {
                read(&k.expr, &mut need);
            }
            let (input, map) = walk(*input, &need);
            let keys = keys
                .into_iter()
                .map(|mut k| {
                    k.expr = moved(&k.expr, &map);
                    k
                })
                .collect();
            (
                Plan::Sort {
                    input: Box::new(input),
                    keys,
                },
                map,
            )
        }
        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            let (input, map) = walk(*input, wanted);
            (
                Plan::Limit {
                    input: Box::new(input),
                    limit,
                    offset,
                },
                map,
            )
        }
        Plan::Aggregate { input, group, aggs } => {
            let mut need = vec![false; input.width()];
            for g in &group {
                read(g, &mut need);
            }
            for a in &aggs {
                a.args.iter().for_each(|x| read(x, &mut need));
                a.order.iter().for_each(|k| read(&k.expr, &mut need));
            }
            let (input, imap) = walk(*input, &need);
            let group = group.iter().map(|g| moved(g, &imap)).collect();
            let aggs = aggs
                .into_iter()
                .map(|mut a| {
                    a.args = a.args.iter().map(|x| moved(x, &imap)).collect();
                    for k in &mut a.order {
                        k.expr = moved(&k.expr, &imap);
                    }
                    a
                })
                .collect();
            (
                Plan::Aggregate {
                    input: Box::new(input),
                    group,
                    aggs,
                },
                identity(w),
            )
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
            algo,
        } => join(*left, *right, kind, on, algo, wanted),
        // Row comparisons, shared and recursive plans, partitions' guards:
        // every column, as they are.
        other => (other, identity(w)),
    }
}

/// A join input narrowed to `need`: the plan, and where its columns are.
fn side(plan: Plan, need: &[bool]) -> (Plan, Vec<Option<usize>>) {
    let w = plan.width();
    let (plan, map) = walk(plan, need);
    let kept: Vec<usize> = (0..w).filter(|&c| need[c]).collect();
    if kept.len() == plan.width() || kept.is_empty() {
        return (plan, map);
    }
    // Cut to the columns needed, in order.
    let mut out = vec![None; w];
    let exprs = kept
        .iter()
        .enumerate()
        .map(|(at, &c)| {
            out[c] = Some(at);
            Expr::Column(map[c].expect("needed"))
        })
        .collect();
    (
        Plan::Project {
            input: Box::new(plan),
            exprs,
        },
        out,
    )
}

fn join(
    left: Plan,
    right: Plan,
    kind: JoinType,
    on: Option<Expr>,
    algo: JoinAlgo,
    wanted: &[bool],
) -> (Plan, Vec<Option<usize>>) {
    let (lw, rw) = (left.width(), right.width());
    let existence = matches!(kind, JoinType::Semi | JoinType::Anti);
    let mut need = vec![false; lw + rw];
    need[..wanted.len().min(lw + rw)].copy_from_slice(&wanted[..wanted.len().min(lw + rw)]);
    if let Some(on) = &on {
        read(on, &mut need);
    }
    let mut left_need = need[..lw].to_vec();
    let mut right_need = need[lw..].to_vec();
    match &algo {
        JoinAlgo::Hash {
            left_keys,
            right_keys,
        } => {
            left_keys.iter().for_each(|k| read(k, &mut left_need));
            right_keys.iter().for_each(|k| read(k, &mut right_need));
        }
        // The right side reads the left row (as `Outer` at depth 1).
        JoinAlgo::Lookup => {
            let mut refs = Vec::new();
            outer_refs(&right, 1, &mut refs);
            for c in refs {
                if let Some(n) = left_need.get_mut(c) {
                    *n = true;
                }
            }
        }
        JoinAlgo::NestedLoop => {}
    }
    let (left, lmap) = side(left, &left_need);
    let (mut right, rmap) = side(right, &right_need);
    let nlw = left.width();
    if algo == JoinAlgo::Lookup {
        right.remap_level(1, &|c| Expr::Column(lmap[c].expect("read by the lookup")));
    }
    let joined = |c: usize| {
        if c < lw {
            lmap[c]
        } else {
            rmap[c - lw].map(|r| nlw + r)
        }
    };
    let on =
        on.map(|e| e.map_columns(&|c| Expr::Column(joined(c).expect("read by the condition"))));
    let algo = match algo {
        JoinAlgo::Hash {
            left_keys,
            right_keys,
        } => JoinAlgo::Hash {
            left_keys: left_keys.iter().map(|k| moved(k, &lmap)).collect(),
            right_keys: right_keys.iter().map(|k| moved(k, &rmap)).collect(),
        },
        a => a,
    };
    let map = if existence {
        lmap
    } else {
        (0..lw + rw).map(joined).collect()
    };
    (
        Plan::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind,
            on,
            algo,
        },
        map,
    )
}

/// The columns of the row `nesting` levels above `plan` that it reads.
fn outer_refs(plan: &Plan, nesting: usize, out: &mut Vec<usize>) {
    plan.for_each_expr(&mut |e| {
        e.walk(&mut |x| match x {
            Expr::Outer { depth, index } if *depth == nesting => out.push(*index),
            Expr::Subquery(s) => outer_refs(&s.plan, nesting + 1, out),
            _ => {}
        })
    });
    for (c, below) in plan.child_levels() {
        outer_refs(c, nesting + below, out);
    }
}
