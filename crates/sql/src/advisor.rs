//! The index advisor (PLAN.md §M14.9), for `nc_catalog.index_advice`:
//! from the statement statistics ([`crate::statements`]) and the tables'
//! `ANALYZE` data, the indexes that would have paid off.
//!
//! Each recorded query, `UPDATE` and `DELETE` (the text of its last run)
//! is planned again.  Its full scans of analyzed tables of at least
//! [`crate::lint::SMALL`] rows give candidates — the columns its filter
//! bounds, equalities first, then one range.  Each candidate is added to
//! its table as a hypothetical index ([`catalog::with_hypothetical`]) and
//! every statement planned once more: where the optimizer picks it and the
//! estimated rows read fall by a tenth or more, it is on offer.  Each
//! statement counts — weighted by its calls — for the candidate that
//! serves it best (on a tie, the one of more columns, which serves more),
//! so one index stands for the statements one index can serve.  The
//! advice: by rows saved, most first.

use std::collections::BTreeSet;

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, Statement};
use crate::binder::{Binder, conjunction, conjuncts};
use crate::catalog::{self, Index, Table};
use crate::error::Result;
use crate::expr::Expr;
use crate::ext::Registry;
use crate::optimizer::{column_bound, optimize};
use crate::plan::{Access, Plan};
use crate::stats::Stats;

/// One index the workload would have used.
#[derive(Clone, Debug, PartialEq)]
pub struct Advice {
    /// The table.
    pub table: String,
    /// Its columns, in index order.
    pub columns: Vec<String>,
    /// The statement that makes it.
    pub create: String,
    /// The normalized statements it helps.
    pub statements: u64,
    /// Their calls.
    pub calls: u64,
    /// The rows they read (estimated, over their calls) as planned now …
    pub rows_read: f64,
    /// … and with the index.
    pub rows_read_with: f64,
    /// The statement it helps most (normalized).
    pub example: String,
    /// That statement's plan with the index.
    pub plan: Vec<String>,
}

/// The plan of a recorded statement, if it is one the advisor plans.
fn plan_of(txn: &dyn KvTxn, reg: &Registry, stmt: &Statement) -> Option<Plan> {
    match stmt {
        Statement::Query(q) => {
            let bound = Binder::new(txn, reg).checked().query(q).ok()?;
            Some(optimize(bound.plan, txn))
        }
        Statement::Update { table, filter, .. } | Statement::Delete { table, filter, .. } => {
            crate::dml::explain_scan(txn, reg, table, filter.as_ref()).ok()
        }
        _ => None,
    }
}

/// Every plan node, subqueries' included.
fn nodes<'a>(plan: &'a Plan, out: &mut Vec<&'a Plan>) {
    out.push(plan);
    plan.for_each_expr(&mut |e| {
        e.walk(&mut |x| {
            if let Expr::Subquery(s) = x {
                nodes(&s.plan, out);
            }
        })
    });
    for c in plan.children() {
        nodes(c, out);
    }
}

/// The rows `plan`'s scans read (estimated; a correlated subquery's once).
fn reads(plan: &Plan, st: &Stats) -> f64 {
    let mut all = Vec::new();
    nodes(plan, &mut all);
    all.iter()
        .map(|p| match p {
            Plan::Scan {
                table,
                access: Access::Full,
                ..
            } => st.rows(table),
            Plan::Scan {
                table,
                access:
                    Access::Key {
                        index,
                        prefix,
                        lower,
                        upper,
                    },
                ..
            } => {
                let cols = match index {
                    None => &table.primary_key,
                    Some(i) => &table.indexes[*i].columns,
                };
                let at = |c: usize| Box::new(Expr::Column(c));
                let mut conds: Vec<Expr> = prefix
                    .iter()
                    .zip(cols)
                    .map(|(e, &c)| Expr::Binary(BinaryOp::Eq, at(c), Box::new(e.clone())))
                    .collect();
                if let Some(&next) = cols.get(prefix.len()) {
                    use std::ops::Bound::*;
                    if let Included(e) | Excluded(e) = lower {
                        conds.push(Expr::Binary(BinaryOp::GtEq, at(next), Box::new(e.clone())));
                    }
                    if let Included(e) | Excluded(e) = upper {
                        conds.push(Expr::Binary(BinaryOp::LtEq, at(next), Box::new(e.clone())));
                    }
                }
                let sel = conjunction(conds).map_or(1.0, |p| st.selectivity(table, &p));
                (st.rows(table) * sel).max(1.0)
            }
            _ => 0.0,
        })
        .sum()
}

/// The candidates a plan's full scans give: (table, columns).
fn candidates(plan: &Plan, st: &Stats, out: &mut BTreeSet<(String, Vec<usize>)>) {
    let mut all = Vec::new();
    nodes(plan, &mut all);
    for p in all {
        let Plan::Scan {
            table,
            access: Access::Full,
            filter: Some(f),
            ..
        } = p
        else {
            continue;
        };
        if st.of(table).is_none() || st.rows(table) < crate::lint::SMALL {
            continue;
        }
        let mut preds = Vec::new();
        conjuncts(f.clone(), &mut preds);
        let bounds: Vec<_> = preds.iter().filter_map(column_bound).collect();
        let mut cols: Vec<usize> = Vec::new();
        for (c, op, _) in &bounds {
            if *op == BinaryOp::Eq && !cols.contains(c) {
                cols.push(*c);
            }
        }
        if let Some((c, ..)) = bounds
            .iter()
            .find(|(c, op, _)| *op != BinaryOp::Eq && !cols.contains(c))
        {
            cols.push(*c);
        }
        if !cols.is_empty() {
            out.insert((table.name.clone(), cols));
        }
    }
}

/// Whether `plan` scans `table` through its index `id`.
fn uses(plan: &Plan, table: &str, id: u32) -> bool {
    let mut all = Vec::new();
    nodes(plan, &mut all);
    all.iter().any(|p| {
        matches!(p, Plan::Scan { table: t, access: Access::Key { index: Some(i), .. }, .. }
            if t.name == table && t.indexes[*i].id == id)
    })
}

/// The advice for the statements of `stats` (the caller's to see).
pub fn advise(
    txn: &dyn KvTxn,
    reg: &Registry,
    stats: &[crate::statements::Stat],
) -> Result<Vec<Advice>> {
    let st = Stats::new(txn);
    // Each statement planned as things are.
    let mut planned = Vec::new();
    let mut cands = BTreeSet::new();
    for s in stats {
        let Ok(mut parsed) = crate::parser::parse(&s.sample) else {
            continue;
        };
        if parsed.len() != 1 {
            continue;
        }
        let stmt = parsed.remove(0);
        let Some(plan) = plan_of(txn, reg, &stmt) else {
            continue;
        };
        candidates(&plan, &st, &mut cands);
        planned.push((s, stmt, reads(&plan, &st)));
    }
    // Each candidate tried on every statement: (candidate, statement,
    // rows read with it, its plan) where the optimizer picks it and it
    // saves a tenth or more.
    let mut offers = Vec::new();
    let mut advice = Vec::new();
    for (name, cols) in cands {
        let Some(table) = catalog::get_table(txn, &name)? else {
            continue;
        };
        let id = table.next_index_id;
        let names: Vec<String> = cols
            .iter()
            .map(|&c| table.columns[c].name.clone())
            .collect();
        let mut hypothetical: Table = table.clone();
        hypothetical.indexes.push(Index::plain(
            id,
            format!("{name}_{}_idx", names.join("_")),
            cols.clone(),
            false,
        ));
        let candidate = advice.len();
        advice.push(Advice {
            table: name.clone(),
            columns: names.clone(),
            create: format!("CREATE INDEX ON {name} ({})", names.join(", ")),
            statements: 0,
            calls: 0,
            rows_read: 0.0,
            rows_read_with: 0.0,
            example: String::new(),
            plan: Vec::new(),
        });
        catalog::with_hypothetical(hypothetical, || {
            for (n, (_, stmt, before)) in planned.iter().enumerate() {
                let Some(plan) = plan_of(txn, reg, stmt) else {
                    continue;
                };
                let after = reads(&plan, &st);
                if uses(&plan, &name, id) && after < before * 0.9 {
                    offers.push((candidate, n, after, plan.explain()));
                }
            }
        });
    }
    // Each statement counts for the candidate that serves it best (on a
    // tie, the one of more columns: it serves more).
    let mut best: Vec<Option<usize>> = vec![None; planned.len()];
    for (o, (c, n, after, _)) in offers.iter().enumerate() {
        let better = match best[*n] {
            None => true,
            Some(b) => {
                let (bc, _, b_after, _) = &offers[b];
                *after < b_after * 0.99
                    || (*after <= b_after * 1.01
                        && advice[*c].columns.len() > advice[*bc].columns.len())
            }
        };
        if better {
            best[*n] = Some(o);
        }
    }
    let mut most = vec![0.0f64; advice.len()];
    for (n, o) in best.iter().enumerate() {
        let Some(o) = o else { continue };
        let (c, _, after, plan) = &offers[*o];
        let (s, _, before) = &planned[n];
        let calls = s.calls as f64;
        let a = &mut advice[*c];
        a.statements += 1;
        a.calls += s.calls;
        a.rows_read += before * calls;
        a.rows_read_with += after * calls;
        if (before - after) * calls > most[*c] {
            most[*c] = (before - after) * calls;
            a.example = s.query.clone();
            a.plan = plan.clone();
        }
    }
    advice.retain(|a| a.statements > 0);
    advice.sort_by(|x, y| {
        let saved = |a: &Advice| a.rows_read - a.rows_read_with;
        saved(y)
            .total_cmp(&saved(x))
            .then_with(|| x.create.cmp(&y.create))
    });
    Ok(advice)
}
