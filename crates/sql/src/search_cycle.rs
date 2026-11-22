//! A recursive CTE's `SEARCH` and `CYCLE` clauses (SQL:1999, PostgreSQL
//! 14; PLAN.md §M14.34 G2): columns added to its rows, as PostgreSQL's
//! rewrite adds them.
//!
//! - `SEARCH DEPTH FIRST BY c… SET s`: `s` the path of `ROW(c…)` from the
//!   base row to this one (`record[]`), so `ORDER BY s` lists a tree depth
//!   first.
//! - `SEARCH BREADTH FIRST BY c… SET s`: `s` `ROW(depth, c…)`, so `ORDER BY
//!   s` lists it level by level (the depth carried in a hidden column).
//! - `CYCLE c… SET m [TO v DEFAULT d] USING p`: `p` the path of `ROW(c…)`,
//!   `m` `v` where this row's `ROW(c…)` is on its parent's path, else `d`;
//!   a marked row is given but not followed (the step reads only rows
//!   marked `d`), so `UNION ALL` ends on a graph with cycles.
//!
//! The base term's rows get the columns from their own (its plan
//! projected); the recursive term — a `SELECT` reading the CTE in its own
//! `FROM` — gets them as expressions over the row it read and the ones it
//! makes, before it is bound.

use crate::ast::{self, BinaryOp, Expr as A, SelectItem, SetExpr, TableRef};
use crate::binder::{Bound, Scope, ScopeCol};
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::plan::Plan;
use crate::types::{DataType, Value};

/// The hidden column carrying a breadth-first search's depth.
const DEPTH: &str = "__search_depth";

/// The two terms with the columns added.
pub(crate) struct Added {
    /// The base term's plan, projected with the columns added.
    pub base: Bound,
    /// The recursive term, with the items and the filter added.
    pub step: SetExpr,
    /// The columns added, each with what it is (for [`check_names`]).
    pub added: Vec<(String, &'static str)>,
}

/// `cte`'s `SEARCH` / `CYCLE` applied: `base` its bound base term, `scope`
/// its columns (named), `step` its recursive term; `mark` `CYCLE`'s
/// `DEFAULT` bound by the caller, with the mark's type (`TO`'s and
/// `DEFAULT`'s common one).
pub(crate) fn apply(
    cte: &ast::Cte,
    base: Bound,
    scope: &Scope,
    step: &SetExpr,
    mark: Option<(Expr, Option<DataType>)>,
) -> Result<Added> {
    let names: Vec<&str> = scope.cols.iter().map(|c| c.name.as_str()).collect();
    let position = |what: &str, c: &str| -> Result<usize> {
        names.iter().position(|n| *n == c).ok_or_else(|| {
            Error::syntax(format!(
                "{what} column \"{c}\" not in WITH query column list"
            ))
        })
    };
    let positions = |what: &str, cols: &[String]| -> Result<Vec<usize>> {
        let mut out: Vec<usize> = Vec::with_capacity(cols.len());
        for c in cols {
            let p = position(what, c)?;
            if out.contains(&p) {
                return Err(Error::duplicate_column(format!(
                    "{what} column \"{c}\" specified more than once"
                )));
            }
            out.push(p);
        }
        Ok(out)
    };
    let search = cte
        .search
        .as_ref()
        .map(|s| Ok::<_, Error>((s, positions("search", &s.by)?)))
        .transpose()?;
    let cycle = cte
        .cycle
        .as_ref()
        .map(|c| Ok::<_, Error>((c, positions("cycle", &c.columns)?)))
        .transpose()?;
    if let (Some((s, _)), Some((c, _))) = (&search, &cycle) {
        if s.set == c.set {
            return Err(Error::syntax(
                "search sequence column name and cycle mark column name are the same",
            ));
        }
        if s.set == c.using {
            return Err(Error::syntax(
                "search sequence column name and cycle path column name are the same",
            ));
        }
    }
    if let Some((c, _)) = &cycle
        && c.set == c.using
    {
        return Err(Error::syntax(
            "cycle mark column name and cycle path column name are the same",
        ));
    }

    // The recursive term: a SELECT reading the CTE in its own FROM.
    let not_top = || {
        Error::feature(format!(
            "with a SEARCH or CYCLE clause, the recursive reference to WITH query \"{}\" must be at the top level of its right-hand SELECT",
            cte.name
        ))
    };
    let SetExpr::Select(sel) = step else {
        return Err(not_top());
    };
    let wt = sel
        .from
        .iter()
        .find_map(|t| reference(t, &cte.name))
        .ok_or_else(not_top)?;
    let mut items = Vec::with_capacity(sel.items.len());
    for item in &sel.items {
        match item {
            SelectItem::Expr { expr, .. } => items.push(expr.clone()),
            _ => {
                return Err(Error::feature(
                    "with a SEARCH or CYCLE clause, the recursive term lists its columns (not *)",
                ));
            }
        }
    }
    if items.len() != names.len() {
        return Err(Error::syntax(
            "each UNION query must have the same number of columns",
        ));
    }
    let theirs = |c: &str| A::Column {
        table: Some(wt.clone()),
        name: c.to_string(),
    };
    let row_of = |ps: &[usize]| A::Row(ps.iter().map(|&p| items[p].clone()).collect());

    let n = names.len();
    let mut cols = scope.cols.clone();
    let mut base_exprs: Vec<Expr> = (0..n).map(Expr::Column).collect();
    let mut step = (**sel).clone();
    let added = |cols: &mut Vec<ScopeCol>, name: &str, ty: Option<DataType>, hidden: bool| {
        cols.push(ScopeCol {
            table: None,
            name: name.to_string(),
            ty,
            hidden,
            origin: None,
            not_null: false,
        });
    };
    let record_of =
        |ps: &[usize]| crate::rows::record(ps.iter().map(|&p| Expr::Column(p)).collect());
    let item = |expr: A| SelectItem::Expr { expr, alias: None };
    let mut depth = false;
    let mut named: Vec<(String, &'static str)> = Vec::new();
    if let Some((s, ps)) = &search {
        if s.breadth {
            // ROW(depth, c…), the depth carried hidden.
            let mut fields = vec![Expr::Literal(Value::Int(0))];
            fields.extend(ps.iter().map(|&p| Expr::Column(p)));
            base_exprs.push(crate::rows::record(fields));
            let next = A::Binary {
                op: BinaryOp::Add,
                left: Box::new(theirs(DEPTH)),
                right: Box::new(A::Literal(Value::Int(1))),
            };
            let mut fields = vec![next];
            fields.extend(ps.iter().map(|&p| items[p].clone()));
            step.items.push(item(A::Row(fields)));
            added(&mut cols, &s.set, Some(DataType::Record), false);
            named.push((s.set.clone(), "search sequence"));
            depth = true;
        } else {
            // The path of ROW(c…).
            base_exprs.push(Expr::Array(vec![record_of(ps)]));
            step.items.push(item(A::Binary {
                op: BinaryOp::Concat,
                left: Box::new(theirs(&s.set)),
                right: Box::new(row_of(ps)),
            }));
            added(&mut cols, &s.set, record_array(), false);
            named.push((s.set.clone(), "search sequence"));
        }
    }
    if let Some((c, ps)) = &cycle {
        let (default, ty) = mark.ok_or_else(|| Error::internal("CYCLE's mark unbound"))?;
        // The mark: TO where this row is on its parent's path.
        base_exprs.push(default);
        step.items.push(item(A::Case {
            operand: None,
            whens: vec![(
                A::AnyAll {
                    left: Box::new(row_of(ps)),
                    op: BinaryOp::Eq,
                    right: Box::new(theirs(&c.using)),
                    all: false,
                },
                c.to.clone(),
            )],
            otherwise: Some(Box::new(c.default.clone())),
        }));
        added(&mut cols, &c.set, ty, false);
        named.push((c.set.clone(), "cycle mark"));
        // The path.
        base_exprs.push(Expr::Array(vec![record_of(ps)]));
        step.items.push(item(A::Binary {
            op: BinaryOp::Concat,
            left: Box::new(theirs(&c.using)),
            right: Box::new(row_of(ps)),
        }));
        added(&mut cols, &c.using, record_array(), false);
        named.push((c.using.clone(), "cycle path"));
        // A marked row is not followed.
        let unmarked = A::Binary {
            op: BinaryOp::NotEq,
            left: Box::new(theirs(&c.set)),
            right: Box::new(c.to.clone()),
        };
        step.filter = Some(match step.filter.take() {
            None => unmarked,
            Some(f) => A::Binary {
                op: BinaryOp::And,
                left: Box::new(f),
                right: Box::new(unmarked),
            },
        });
    }
    if depth {
        base_exprs.push(Expr::Literal(Value::Int(0)));
        step.items.push(item(A::Binary {
            op: BinaryOp::Add,
            left: Box::new(theirs(DEPTH)),
            right: Box::new(A::Literal(Value::Int(1))),
        }));
        added(&mut cols, DEPTH, Some(DataType::BigInt), true);
    }
    Ok(Added {
        base: Bound {
            plan: Plan::Project {
                input: Box::new(base.plan),
                exprs: base_exprs,
            },
            scope: Scope { cols },
        },
        step: SetExpr::Select(Box::new(step)),
        added: named,
    })
}

/// An added column named as one of the query's own (`own`): refused.
pub(crate) fn check_names(own: &[ScopeCol], added: &[(String, &'static str)]) -> Result<()> {
    for (name, what) in added {
        if own.iter().any(|c| &c.name == name) {
            return Err(Error::syntax(format!(
                "{what} column name \"{name}\" already used in WITH query column list"
            )));
        }
    }
    Ok(())
}

/// `record[]`.
fn record_array() -> Option<DataType> {
    DataType::Record.array_of()
}

/// The name a reference to CTE `name` in this `FROM` item is known by.
fn reference(t: &TableRef, name: &str) -> Option<String> {
    match t {
        TableRef::Table {
            schema: None,
            name: n,
            alias,
            ..
        } if n == name => Some(alias.clone().unwrap_or_else(|| n.clone())),
        TableRef::Join { left, right, .. } => {
            reference(left, name).or_else(|| reference(right, name))
        }
        _ => None,
    }
}
