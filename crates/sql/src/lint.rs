//! The linter (PLAN.md §M14.9): `ANALYZE QUERY <statement>` plans a
//! statement without running it and lists what in it is likely slow or
//! wrong — each finding with its rule, where it is and the fix.
//!
//! Rules, over the optimized plan (subqueries included):
//!
//! - `full_scan`: a table read in full for a filter that keeps few of its
//!   rows (a tenth or less, by the statistics or the defaults) and that an
//!   index could serve.  An analyzed table of fewer than [`SMALL`] rows is
//!   left alone.
//! - `index_defeated`: a comparison that wraps an indexed column (the
//!   first of an index) in a function, a cast or arithmetic, so the index
//!   cannot serve it.
//! - `foreign_key_without_index`: a foreign key of a table the statement
//!   reads — or, for `UPDATE` and `DELETE`, one referencing the table it
//!   changes — whose columns lead no index: every change of the
//!   referenced row scans the referencing table.
//! - `cartesian_product`: a join without a condition between two inputs
//!   of more than one row.
//! - `not_in_nullable`: `NOT IN (subquery)` over a column that can be
//!   NULL: one NULL makes it unknown for every row.

use std::collections::HashSet;

use nucleus_kv::KvTxn;

use crate::ast::BinaryOp;
use crate::binder::conjuncts;
use crate::catalog::{self, Table};
use crate::error::Result;
use crate::expr::{Expr, SubKind};
use crate::optimizer::{column_bound, estimate, origin};
use crate::plan::{Access, JoinType, Plan, show_with};
use crate::stats::Stats;

/// An analyzed table smaller than this is not worth an index.
pub const SMALL: f64 = 1000.0;

/// A finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// The rule.
    pub rule: &'static str,
    /// Where: the table, the join, the condition.
    pub place: String,
    /// What is wrong.
    pub finding: String,
    /// What to do.
    pub fix: String,
}

/// The findings for `plan`; `changed`: the table an `UPDATE` or `DELETE`
/// changes.
pub fn lint(txn: &dyn KvTxn, plan: &Plan, changed: Option<&Table>) -> Result<Vec<Finding>> {
    let st = Stats::new(txn);
    let mut l = Linter {
        st: &st,
        out: Vec::new(),
        tables: HashSet::new(),
    };
    l.visit(plan);
    if let Some(t) = changed {
        for (child, _) in &t.referenced_by {
            if child == &t.name {
                continue; // its own scan covered it
            }
            if let Some(c) = catalog::get_table(txn, child)? {
                l.foreign_keys(&c, Some(&t.name));
            }
        }
    }
    Ok(l.out)
}

struct Linter<'a> {
    st: &'a Stats<'a>,
    out: Vec<Finding>,
    /// Tables whose foreign keys were looked at.
    tables: HashSet<String>,
}

fn named(table: &Table) -> impl Fn(usize) -> String + '_ {
    |i| {
        table
            .columns
            .get(i)
            .map_or_else(|| format!("#{i}"), |c| c.name.clone())
    }
}

/// The index (`None`: the primary key) whose first column is `col`.
fn index_led_by(table: &Table, col: usize) -> Option<Option<usize>> {
    if table.primary_key.first() == Some(&col) {
        return Some(None);
    }
    table
        .indexes
        .iter()
        .position(|ix| !ix.mapped() && ix.columns.first() == Some(&col))
        .map(Some)
}

fn index_name(table: &Table, index: Option<usize>) -> String {
    match index {
        None => format!("the primary key of {}", table.name),
        Some(i) => format!("index {}", table.indexes[i].name),
    }
}

fn row_free(e: &Expr) -> bool {
    e.columns().is_empty() && !e.has_subquery()
}

/// The tables an input reads, for naming it.
fn tables_of(plan: &Plan) -> String {
    fn walk(p: &Plan, out: &mut Vec<String>) {
        match p {
            Plan::Scan { table, .. } => out.push(table.name.clone()),
            Plan::Cte { name, .. } | Plan::WorkTable { name, .. } => out.push(name.clone()),
            Plan::View { view } => out.push(format!("{}.{}", view.schema(), view.name())),
            p => p.children().into_iter().for_each(|c| walk(c, out)),
        }
    }
    let mut out = Vec::new();
    walk(plan, &mut out);
    match out.len() {
        0 => "a computed input".into(),
        _ => out.join(", "),
    }
}

/// Whether `pred` holds only where column `col` is not NULL.
fn rejects_null(pred: &Expr, col: usize) -> bool {
    let is_col = |e: &Expr| matches!(e, Expr::Column(c) if *c == col);
    match pred {
        Expr::Binary(op, a, b) if !matches!(op, BinaryOp::Or | BinaryOp::And) => {
            is_col(a) || is_col(b)
        }
        Expr::Binary(BinaryOp::And, a, b) => rejects_null(a, col) || rejects_null(b, col),
        Expr::IsNull(e, true) | Expr::InList(e, _, _) => is_col(e),
        Expr::Like { expr, .. } => is_col(expr),
        _ => false,
    }
}

/// Whether column `col` of `plan`'s output can be NULL (when unsure: yes).
fn nullable(plan: &Plan, col: usize) -> bool {
    match plan {
        Plan::Scan { table, filter, .. } => {
            let c = &table.columns[col];
            c.nullable
                && !table.primary_key.contains(&col)
                && !filter.as_ref().is_some_and(|f| rejects_null(f, col))
        }
        Plan::Filter { input, predicate } => !rejects_null(predicate, col) && nullable(input, col),
        Plan::Secure { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input } => nullable(input, col),
        Plan::Cte { plan, .. } => nullable(plan, col),
        Plan::Project { input, exprs } => match &exprs[col] {
            Expr::Column(i) => nullable(input, *i),
            Expr::Literal(v) => v.is_null(),
            _ => true,
        },
        Plan::Join {
            left, right, kind, ..
        } => {
            let lw = left.width();
            if col < lw {
                *kind == JoinType::Full || nullable(left, col)
            } else {
                *kind != JoinType::Inner || nullable(right, col - lw)
            }
        }
        Plan::Aggregate { input, group, aggs } => match group.get(col) {
            Some(Expr::Column(i)) => nullable(input, *i),
            Some(_) => true,
            None => aggs[col - group.len()].func.name() != "count",
        },
        Plan::SetOp { left, right, .. } => nullable(left, col) || nullable(right, col),
        Plan::Values { rows, .. } => rows
            .iter()
            .any(|r| !matches!(&r[col], Expr::Literal(v) if !v.is_null())),
        _ => true,
    }
}

impl Linter<'_> {
    fn visit(&mut self, plan: &Plan) {
        match plan {
            Plan::Scan {
                table,
                access,
                filter,
                ..
            } => {
                if let Some(f) = filter {
                    self.scan(table, access, f);
                }
                self.foreign_keys(table, None);
            }
            Plan::Join {
                left, right, on, ..
            } => {
                let unconditioned = match on {
                    None => true,
                    Some(Expr::Literal(v)) => *v == crate::types::Value::Bool(true),
                    Some(_) => false,
                };
                let (l, r) = (estimate(left, self.st), estimate(right, self.st));
                if unconditioned && l > 1.0 && r > 1.0 {
                    let (ln, rn) = (tables_of(left), tables_of(right));
                    self.out.push(Finding {
                        rule: "cartesian_product",
                        place: format!("the join of {ln} and {rn}"),
                        finding: format!(
                            "every row of one side is paired with every row of the other \
                             (about {:.0} × {:.0} rows)",
                            l.max(1.0),
                            r.max(1.0)
                        ),
                        fix: "join them on a condition (JOIN … ON …); if every pair is \
                              meant, ignore this"
                            .into(),
                    });
                }
            }
            _ => {}
        }
        let mut subs = Vec::new();
        plan.for_each_expr(&mut |e| {
            e.walk(&mut |x| {
                if let Expr::Subquery(s) = x {
                    subs.push(&**s);
                }
            })
        });
        for s in subs {
            if let SubKind::In(_, true) = s.kind
                && nullable(&s.plan, 0)
            {
                let column = origin(&s.plan, 0).map_or_else(
                    || "its value".to_string(),
                    |(t, c)| format!("{}.{}", t.name, t.columns[c].name),
                );
                self.out.push(Finding {
                    rule: "not_in_nullable",
                    place: format!("NOT IN (a subquery on {})", tables_of(&s.plan)),
                    finding: format!(
                        "the subquery's {column} can be NULL: one NULL makes NOT IN unknown \
                         for every row, so it keeps none"
                    ),
                    fix: format!(
                        "write NOT EXISTS (SELECT 1 FROM … WHERE … = …), or add WHERE \
                         {column} IS NOT NULL to the subquery"
                    ),
                });
            }
            self.visit(&s.plan);
        }
        for c in plan.children() {
            self.visit(c);
        }
    }

    fn scan(&mut self, table: &Table, access: &Access, filter: &Expr) {
        let name = named(table);
        let mut preds = Vec::new();
        conjuncts(filter.clone(), &mut preds);
        let used = match access {
            Access::Full => None,
            Access::Key { index, .. } => Some(*index),
        };
        // index_defeated
        for p in &preds {
            let Expr::Binary(op, a, b) = p else { continue };
            if !matches!(
                op,
                BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
            ) {
                continue;
            }
            for (side, other) in [(a, b), (b, a)] {
                let cols: HashSet<usize> = side.columns().into_iter().collect();
                if matches!(**side, Expr::Column(_)) || !row_free(other) || cols.len() != 1 {
                    continue;
                }
                let col = *cols.iter().next().expect("one");
                let Some(index) = index_led_by(table, col) else {
                    continue;
                };
                if used == Some(index) {
                    continue;
                }
                let cname = &table.columns[col].name;
                // What is done to the column, under the casts the binder
                // adds (`(id + 1)::integer`).
                let mut inner = &**side;
                while let Expr::Cast(e, _) = inner
                    && !matches!(**e, Expr::Column(_))
                {
                    inner = e;
                }
                let fix = match inner {
                    Expr::Cast(_, _) => format!(
                        "compare {cname} with a value of its own type ({}): cast the other \
                         side, not the column",
                        table.columns[col].ty
                    ),
                    Expr::Binary(
                        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div,
                        _,
                        _,
                    ) => format!("move the arithmetic to the other side, leaving {cname} alone"),
                    _ => format!(
                        "compare {cname} itself, or keep the computed value in a column of \
                         its own and index that"
                    ),
                };
                self.out.push(Finding {
                    rule: "index_defeated",
                    place: format!("{}: {}", table.name, show_with(p, &name)),
                    finding: format!(
                        "the comparison is on an expression of {cname}, not on {cname} \
                         itself, so {} cannot serve it",
                        index_name(table, index)
                    ),
                    fix,
                });
            }
        }
        // full_scan
        if used.is_some() {
            return;
        }
        let bounds: Vec<(usize, BinaryOp, Expr)> = preds.iter().filter_map(column_bound).collect();
        if bounds.is_empty() {
            return;
        }
        let analyzed = self.st.of(table).is_some();
        let rows = self.st.rows(table);
        let sel = self.st.selectivity(table, filter);
        if (analyzed && rows < SMALL) || sel > 0.1 {
            return;
        }
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
        let list = cols.iter().map(|&c| name(c)).collect::<Vec<_>>().join(", ");
        self.out.push(Finding {
            rule: "full_scan",
            place: format!("{}: {}", table.name, show_with(filter, &name)),
            finding: format!(
                "every row of {} is read (about {:.0}{}) to keep about {:.0}",
                table.name,
                rows,
                if analyzed { "" } else { ", never analyzed" },
                (rows * sel).max(1.0)
            ),
            fix: format!("CREATE INDEX ON {} ({list})", table.name),
        });
    }

    /// Foreign keys of `table` whose columns lead no index (only those
    /// referencing `to`, if given).
    fn foreign_keys(&mut self, table: &Table, to: Option<&str>) {
        if to.is_none() && !self.tables.insert(table.name.clone()) {
            return;
        }
        for fk in &table.foreign_keys {
            if to.is_some_and(|t| t != fk.table) {
                continue;
            }
            let n = fk.columns.len();
            let mut want = fk.columns.clone();
            want.sort_unstable();
            let leads = |cols: &[usize]| {
                cols.len() >= n && {
                    let mut c = cols[..n].to_vec();
                    c.sort_unstable();
                    c == want
                }
            };
            if leads(&table.primary_key)
                || table
                    .indexes
                    .iter()
                    .any(|ix| !ix.mapped() && leads(&ix.columns))
            {
                continue;
            }
            let name = named(table);
            let list = fk
                .columns
                .iter()
                .map(|&c| name(c))
                .collect::<Vec<_>>()
                .join(", ");
            self.out.push(Finding {
                rule: "foreign_key_without_index",
                place: format!(
                    "{}: foreign key {} ({list}) → {}",
                    table.name, fk.name, fk.table
                ),
                finding: format!(
                    "no index starts with ({list}): every change of a row of {} scans {} in \
                     full, and joins on ({list}) cannot use an index",
                    fk.table, table.name
                ),
                fix: format!("CREATE INDEX ON {} ({list})", table.name),
            });
        }
    }
}
