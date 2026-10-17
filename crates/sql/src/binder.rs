//! The binder: syntax tree → plan, with names resolved, types inferred and
//! aggregates lifted out.

use nucleus_kv::KvTxn;

use crate::ast::{self, BinaryOp, JoinKind, SelectItem, SetExpr, TableRef, UnaryOp};
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::expr::{Expr, Func, SubKind, SubPlan};
use crate::plan::{Access, Agg, AggFunc, JoinAlgo, JoinType, Plan, SortKey};
use crate::types::{DataType, Value};

/// A column visible to expressions.
#[derive(Clone, Debug)]
pub struct ScopeCol {
    /// The table or alias it comes from.
    pub table: Option<String>,
    /// Its name.
    pub name: String,
    /// Its type, if known (`NULL` literals have none).
    pub ty: Option<DataType>,
    /// Left out of `*` (the second copy of a `USING` column).
    pub hidden: bool,
}

/// The columns of a plan's output, by position.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// The columns.
    pub cols: Vec<ScopeCol>,
}

impl Scope {
    fn find(&self, table: Option<&str>, name: &str) -> Result<Option<usize>> {
        let mut found = None;
        for (i, c) in self.cols.iter().enumerate() {
            if c.name == name && table.is_none_or(|t| c.table.as_deref() == Some(t)) {
                if found.is_some() {
                    if table.is_none() && c.hidden {
                        continue; // USING columns: the visible copy wins
                    }
                    return Err(Error::ambiguous_column(format!(
                        "column reference \"{name}\" is ambiguous"
                    )));
                }
                found = Some(i);
            }
        }
        Ok(found)
    }

    fn has_table(&self, t: &str) -> bool {
        self.cols.iter().any(|c| c.table.as_deref() == Some(t))
    }
}

/// A bound query: its plan and its output columns.
#[derive(Clone, Debug)]
pub struct Bound {
    /// The plan.
    pub plan: Plan,
    /// The output columns.
    pub scope: Scope,
}

/// Aggregation state while binding a grouped `SELECT`.
struct Grouping {
    from: Scope,
    group: Vec<Expr>,
    group_types: Vec<Option<DataType>>,
    aggs: Vec<Agg>,
    agg_types: Vec<Option<DataType>>,
}

/// The binder.
pub struct Binder<'a> {
    txn: &'a dyn KvTxn,
    /// Enclosing queries' scopes, innermost last.
    outer: Vec<Scope>,
    /// Inferred parameter types, by `$n - 1`.
    pub params: Vec<Option<DataType>>,
}

type Typed = (Expr, Option<DataType>);

fn is_aggregate(name: &str) -> bool {
    AggFunc::lookup(name).is_some()
}

/// Whether `e` calls an aggregate (not looking into subqueries).
fn contains_agg(e: &ast::Expr) -> bool {
    use ast::Expr as A;
    match e {
        A::Function { name, args, .. } => is_aggregate(name) || args.iter().any(contains_agg),
        A::Unary { expr, .. } | A::IsNull { expr, .. } | A::Cast { expr, .. } => contains_agg(expr),
        A::Binary { left, right, .. } => contains_agg(left) || contains_agg(right),
        A::Between {
            expr, low, high, ..
        } => contains_agg(expr) || contains_agg(low) || contains_agg(high),
        A::InList { expr, list, .. } => contains_agg(expr) || list.iter().any(contains_agg),
        A::InSubquery { expr, .. } => contains_agg(expr),
        A::Like { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        A::Case {
            operand,
            whens,
            otherwise,
        } => {
            operand.as_deref().is_some_and(contains_agg)
                || whens
                    .iter()
                    .any(|(w, t)| contains_agg(w) || contains_agg(t))
                || otherwise.as_deref().is_some_and(contains_agg)
        }
        A::Literal(_) | A::Column { .. } | A::Param(_) | A::Exists { .. } | A::Subquery(_) => false,
    }
}

/// The output name of a select-list expression without an alias.
fn default_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Column { name, .. } => name.clone(),
        ast::Expr::Function { name, .. } if !name.starts_with("__") => name.clone(),
        ast::Expr::Cast { expr, .. } => default_name(expr),
        ast::Expr::Case { .. } => "case".into(),
        ast::Expr::Exists { .. } => "exists".into(),
        _ => "?column?".into(),
    }
}

/// `x AND y AND …` split into conjuncts.
pub fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Binary(BinaryOp::And, l, r) => {
            conjuncts(*l, out);
            conjuncts(*r, out);
        }
        e => out.push(e),
    }
}

/// Conjuncts joined back with `AND`.
pub fn conjunction(mut es: Vec<Expr>) -> Option<Expr> {
    let first = if es.is_empty() {
        return None;
    } else {
        es.remove(0)
    };
    Some(es.into_iter().fold(first, |a, b| {
        Expr::Binary(BinaryOp::And, Box::new(a), Box::new(b))
    }))
}

impl<'a> Binder<'a> {
    /// A binder reading the catalog through `txn`.
    pub fn new(txn: &'a dyn KvTxn) -> Self {
        Self {
            txn,
            outer: Vec::new(),
            params: Vec::new(),
        }
    }

    /// The transaction the binder reads the catalog through.
    pub fn txn(&self) -> &'a dyn KvTxn {
        self.txn
    }

    fn table(&self, name: &str) -> Result<Table> {
        catalog::table(self.txn, name)
    }

    /// Records the type of parameter `$n+1`, unless already known.
    pub fn note_param(&mut self, n: usize, ty: Option<DataType>) {
        if self.params.len() <= n {
            self.params.resize(n + 1, None);
        }
        if self.params[n].is_none() {
            self.params[n] = ty;
        }
    }

    // ---------------------------------------------------------------- queries

    /// Binds a query.
    pub fn query(&mut self, q: &ast::Query) -> Result<Bound> {
        if let SetExpr::Select(s) = &q.body {
            return self.select(s, &q.order_by, q.limit.as_ref(), q.offset.as_ref());
        }
        let mut bound = self.set_expr(&q.body)?;
        if !q.order_by.is_empty() {
            let mut keys = Vec::new();
            for item in &q.order_by {
                let i = self
                    .output_position(&item.expr, &bound.scope)?
                    .ok_or_else(|| {
                        Error::feature(
                            "ORDER BY on a set operation or VALUES must name an output column or position",
                        )
                    })?;
                keys.push(SortKey {
                    expr: Expr::Column(i),
                    desc: item.desc,
                    nulls_first: item.nulls_first.unwrap_or(item.desc),
                });
            }
            bound.plan = Plan::Sort {
                input: Box::new(bound.plan),
                keys,
            };
        }
        self.limit(bound, q.limit.as_ref(), q.offset.as_ref())
    }

    fn limit(
        &mut self,
        mut b: Bound,
        limit: Option<&ast::Expr>,
        offset: Option<&ast::Expr>,
    ) -> Result<Bound> {
        if limit.is_some() || offset.is_some() {
            let empty = Scope::default();
            let mut bind = |e: &ast::Expr| -> Result<Expr> {
                let (x, t) = self.expr(e, &empty)?;
                if let Expr::Param(n) = x {
                    self.note_param(n, Some(DataType::Int));
                } else if t.is_some_and(|t| t != DataType::Int) {
                    return Err(Error::type_mismatch("LIMIT and OFFSET take integers"));
                }
                Ok(x)
            };
            let limit = limit.map(&mut bind).transpose()?;
            let offset = offset.map(&mut bind).transpose()?;
            b.plan = Plan::Limit {
                input: Box::new(b.plan),
                limit,
                offset,
            };
        }
        Ok(b)
    }

    /// An `ORDER BY` item naming an output column: a position or a name.
    fn output_position(&self, e: &ast::Expr, scope: &Scope) -> Result<Option<usize>> {
        match e {
            ast::Expr::Literal(Value::Int(n)) => {
                let n = *n;
                if n < 1 || n as usize > scope.cols.len() {
                    return Err(Error::undefined_column(format!(
                        "ORDER BY position {n} is not in select list"
                    )));
                }
                Ok(Some(n as usize - 1))
            }
            ast::Expr::Column { table: None, name } => {
                Ok(scope.cols.iter().position(|c| &c.name == name))
            }
            _ => Ok(None),
        }
    }

    fn set_expr(&mut self, body: &SetExpr) -> Result<Bound> {
        match body {
            SetExpr::Select(s) => self.select(s, &[], None, None),
            SetExpr::Values(rows) => self.values(rows),
            SetExpr::SetOp {
                op,
                left,
                right,
                all,
            } => {
                let l = self.set_expr(left)?;
                let r = self.set_expr(right)?;
                if l.scope.cols.len() != r.scope.cols.len() {
                    return Err(Error::syntax(format!(
                        "each {} query must have the same number of columns",
                        op.keyword()
                    )));
                }
                let mut scope = l.scope.clone();
                for (c, rc) in scope.cols.iter_mut().zip(&r.scope.cols) {
                    c.ty = unify(c.ty, rc.ty)?;
                    c.table = None;
                }
                Ok(Bound {
                    plan: Plan::SetOp {
                        op: *op,
                        left: Box::new(l.plan),
                        right: Box::new(r.plan),
                        all: *all,
                    },
                    scope,
                })
            }
        }
    }

    fn values(&mut self, rows: &[Vec<ast::Expr>]) -> Result<Bound> {
        let width = rows.first().map_or(0, Vec::len);
        let mut types: Vec<Option<DataType>> = vec![None; width];
        let mut out = Vec::with_capacity(rows.len());
        let empty = Scope::default();
        for row in rows {
            if row.len() != width {
                return Err(Error::syntax("VALUES lists must all be the same length"));
            }
            let mut bound = Vec::with_capacity(width);
            for (i, e) in row.iter().enumerate() {
                let (x, t) = self.expr(e, &empty)?;
                types[i] = unify(types[i], t)?;
                bound.push(x);
            }
            out.push(bound);
        }
        // Mixed int/float columns become float.
        for row in &mut out {
            for (i, x) in row.iter_mut().enumerate() {
                if types[i] == Some(DataType::Float) {
                    *x = Expr::Cast(Box::new(x.clone()), DataType::Float);
                }
            }
        }
        let scope = Scope {
            cols: types
                .iter()
                .enumerate()
                .map(|(i, t)| ScopeCol {
                    table: None,
                    name: format!("column{}", i + 1),
                    ty: *t,
                    hidden: false,
                })
                .collect(),
        };
        Ok(Bound {
            plan: Plan::Values { rows: out, width },
            scope,
        })
    }

    // ------------------------------------------------------------------ FROM

    fn from(&mut self, items: &[TableRef]) -> Result<Bound> {
        let mut acc: Option<Bound> = None;
        for item in items {
            let b = self.table_ref(item)?;
            acc = Some(match acc {
                None => b,
                Some(a) => self.join(a, b, JoinType::Inner, None)?,
            });
        }
        // No FROM: one empty row.
        Ok(acc.unwrap_or(Bound {
            plan: Plan::Values {
                rows: vec![vec![]],
                width: 0,
            },
            scope: Scope::default(),
        }))
    }

    fn join(&mut self, l: Bound, r: Bound, kind: JoinType, on: Option<Expr>) -> Result<Bound> {
        let mut scope = l.scope;
        scope.cols.extend(r.scope.cols);
        Ok(Bound {
            plan: Plan::Join {
                left: Box::new(l.plan),
                right: Box::new(r.plan),
                kind,
                on,
                algo: JoinAlgo::NestedLoop,
            },
            scope,
        })
    }

    fn table_ref(&mut self, t: &TableRef) -> Result<Bound> {
        match t {
            TableRef::Table {
                name,
                alias,
                columns,
            } => {
                let table = self.table(name)?;
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let mut scope = Scope {
                    cols: table
                        .columns
                        .iter()
                        .map(|c| ScopeCol {
                            table: Some(qual.clone()),
                            name: c.name.clone(),
                            ty: Some(c.ty),
                            hidden: false,
                        })
                        .collect(),
                };
                rename_columns(&mut scope, &qual, columns)?;
                Ok(Bound {
                    plan: Plan::Scan {
                        table,
                        access: Access::Full,
                        filter: None,
                        estimate: None,
                    },
                    scope,
                })
            }
            TableRef::Subquery {
                query,
                alias,
                columns,
            } => {
                let mut b = self.query(query)?;
                for c in &mut b.scope.cols {
                    c.table = Some(alias.clone());
                }
                rename_columns(&mut b.scope, alias, columns)?;
                Ok(b)
            }
            TableRef::Join {
                left,
                right,
                kind,
                on,
                using,
            } => {
                let (l, r) = (self.table_ref(left)?, self.table_ref(right)?);
                // RIGHT JOIN = LEFT JOIN with the sides swapped, then the
                // columns put back in order.
                let (l, r, swapped) = if *kind == JoinKind::Right {
                    (r, l, true)
                } else {
                    (l, r, false)
                };
                let (lw, rw) = (l.scope.cols.len(), r.scope.cols.len());
                let jt = match kind {
                    JoinKind::Inner => JoinType::Inner,
                    JoinKind::Left | JoinKind::Right => JoinType::Left,
                    JoinKind::Full => JoinType::Full,
                };
                let mut combined = l.scope.clone();
                combined.cols.extend(r.scope.cols.clone());
                let mut merged = Vec::new();
                let cond = if !using.is_empty() {
                    let mut parts = Vec::new();
                    for col in using {
                        let li = l.scope.find(None, col)?.ok_or_else(|| Error::undefined_column(format!("column \"{col}\" specified in USING clause does not exist in left table")))?;
                        let ri = r.scope.find(None, col)?.ok_or_else(|| Error::undefined_column(format!("column \"{col}\" specified in USING clause does not exist in right table")))?;
                        let (lt, rt) = (l.scope.cols[li].ty, r.scope.cols[ri].ty);
                        comparable(lt, rt)?;
                        parts.push(Expr::Binary(
                            BinaryOp::Eq,
                            Box::new(Expr::Column(li)),
                            Box::new(Expr::Column(lw + ri)),
                        ));
                        // The copy from the (possibly NULL-padded) inner
                        // side stays addressable by its table but leaves
                        // `*` and unqualified names.
                        combined.cols[lw + ri].hidden = true;
                        if jt == JoinType::Full {
                            // Both sides may be NULL-padded: the merged
                            // column is the first non-NULL of the two.
                            combined.cols[li].hidden = true;
                            merged.push((col.clone(), li, lw + ri, unify(lt, rt)?));
                        }
                    }
                    conjunction(parts)
                } else {
                    on.as_ref()
                        .map(|e| self.condition(e, &combined))
                        .transpose()?
                };
                let mut b = Bound {
                    plan: Plan::Join {
                        left: Box::new(l.plan),
                        right: Box::new(r.plan),
                        kind: jt,
                        on: cond,
                        algo: JoinAlgo::NestedLoop,
                    },
                    scope: combined,
                };
                if !merged.is_empty() {
                    // FULL JOIN … USING: the merged columns first, then
                    // every input column (the USING copies hidden).
                    let mut exprs = Vec::new();
                    let mut cols = Vec::new();
                    for (name, li, ri, ty) in &merged {
                        let side = |i: usize| match ty {
                            Some(DataType::Float) if b.scope.cols[i].ty == Some(DataType::Int) => {
                                Expr::Cast(Box::new(Expr::Column(i)), DataType::Float)
                            }
                            _ => Expr::Column(i),
                        };
                        exprs.push(Expr::Func(Func::Coalesce, vec![side(*li), side(*ri)]));
                        cols.push(ScopeCol {
                            table: None,
                            name: name.clone(),
                            ty: *ty,
                            hidden: false,
                        });
                    }
                    exprs.extend((0..lw + rw).map(Expr::Column));
                    cols.extend(b.scope.cols.iter().cloned());
                    b = Bound {
                        plan: Plan::Project {
                            input: Box::new(b.plan),
                            exprs,
                        },
                        scope: Scope { cols },
                    };
                }
                if swapped {
                    let exprs: Vec<Expr> = (rw..rw + lw).chain(0..rw).map(Expr::Column).collect();
                    let cols: Vec<ScopeCol> = exprs
                        .iter()
                        .map(|e| match e {
                            Expr::Column(i) => b.scope.cols[*i].clone(),
                            _ => unreachable!(),
                        })
                        .collect();
                    b = Bound {
                        plan: Plan::Project {
                            input: Box::new(b.plan),
                            exprs,
                        },
                        scope: Scope { cols },
                    };
                }
                Ok(b)
            }
        }
    }

    // ---------------------------------------------------------------- SELECT

    fn select(
        &mut self,
        s: &ast::Select,
        order_by: &[ast::OrderItem],
        limit: Option<&ast::Expr>,
        offset: Option<&ast::Expr>,
    ) -> Result<Bound> {
        let from = self.from(&s.from)?;
        let mut plan = from.plan;
        let scope = from.scope;
        if let Some(w) = &s.filter {
            if contains_agg(w) {
                return Err(Error::grouping(
                    "aggregate functions are not allowed in WHERE",
                ));
            }
            let predicate = self.condition(w, &scope)?;
            plan = Plan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        let grouped = !s.group_by.is_empty()
            || s.having.is_some()
            || s.items
                .iter()
                .any(|i| matches!(i, SelectItem::Expr { expr, .. } if contains_agg(expr)))
            || order_by.iter().any(|o| contains_agg(&o.expr));
        let mut grouping = None;
        if grouped {
            let mut g = Grouping {
                from: scope.clone(),
                group: vec![],
                group_types: vec![],
                aggs: vec![],
                agg_types: vec![],
            };
            for e in &s.group_by {
                // GROUP BY 1 / GROUP BY alias: select-list items.
                let resolved = self.group_item(e, &s.items)?;
                let (x, t) = self.expr(resolved, &scope)?;
                g.group.push(x);
                g.group_types.push(t);
            }
            grouping = Some(g);
        }

        // The select list.
        let mut exprs = Vec::new();
        let mut cols = Vec::new();
        for item in &s.items {
            match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    let only = match item {
                        SelectItem::QualifiedWildcard(t) => {
                            if !scope.has_table(t) {
                                return Err(Error::undefined_table(format!(
                                    "missing FROM-clause entry for table \"{t}\""
                                )));
                            }
                            Some(t.as_str())
                        }
                        _ => None,
                    };
                    if s.from.is_empty() {
                        return Err(Error::syntax(
                            "SELECT * with no tables specified is not valid",
                        ));
                    }
                    for (i, c) in scope.cols.iter().enumerate() {
                        if c.hidden || only.is_some_and(|t| c.table.as_deref() != Some(t)) {
                            continue;
                        }
                        let e = match &mut grouping {
                            Some(g) => self.grouped_column(g, i)?,
                            None => Expr::Column(i),
                        };
                        exprs.push(e);
                        cols.push(ScopeCol {
                            table: c.table.clone(),
                            name: c.name.clone(),
                            ty: c.ty,
                            hidden: false,
                        });
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let (x, t) = match &mut grouping {
                        Some(g) => self.grouped_expr(expr, g)?,
                        None => self.expr(expr, &scope)?,
                    };
                    let table = match expr {
                        ast::Expr::Column { .. } if alias.is_none() => match &x {
                            Expr::Column(i) if grouping.is_none() => scope.cols[*i].table.clone(),
                            _ => None,
                        },
                        _ => None,
                    };
                    exprs.push(x);
                    cols.push(ScopeCol {
                        table,
                        name: alias.clone().unwrap_or_else(|| default_name(expr)),
                        ty: t,
                        hidden: false,
                    });
                }
            }
        }
        let visible = exprs.len();

        // HAVING, over the aggregate output.
        let mut having = None;
        if let (Some(h), Some(g)) = (&s.having, &mut grouping) {
            let (x, t) = self.grouped_expr(h, g)?;
            if t.is_some_and(|t| t != DataType::Bool) {
                return Err(Error::type_mismatch(
                    "argument of HAVING must be type boolean",
                ));
            }
            having = Some(x);
        }

        // ORDER BY: output columns by name or position, else expressions
        // over the input, carried as hidden extra columns.
        let mut keys = Vec::new();
        for item in order_by {
            let nulls_first = item.nulls_first.unwrap_or(item.desc);
            if let Some(i) = self.output_position(&item.expr, &Scope { cols: cols.clone() })? {
                keys.push(SortKey {
                    expr: Expr::Column(i),
                    desc: item.desc,
                    nulls_first,
                });
                continue;
            }
            let (x, _) = match &mut grouping {
                Some(g) => self.grouped_expr(&item.expr, g)?,
                None => self.expr(&item.expr, &scope)?,
            };
            let i = match exprs.iter().position(|e| *e == x) {
                Some(i) => i,
                None => {
                    if s.distinct {
                        return Err(Error::feature(
                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                        ));
                    }
                    exprs.push(x);
                    exprs.len() - 1
                }
            };
            keys.push(SortKey {
                expr: Expr::Column(i),
                desc: item.desc,
                nulls_first,
            });
        }

        if let Some(g) = grouping {
            plan = Plan::Aggregate {
                input: Box::new(plan),
                group: g.group,
                aggs: g.aggs,
            };
            if let Some(h) = having {
                plan = Plan::Filter {
                    input: Box::new(plan),
                    predicate: h,
                };
            }
        }
        plan = Plan::Project {
            input: Box::new(plan),
            exprs,
        };
        if s.distinct {
            plan = Plan::Distinct {
                input: Box::new(plan),
            };
        }
        if !keys.is_empty() {
            plan = Plan::Sort {
                input: Box::new(plan),
                keys,
            };
        }
        let mut bound = self.limit(
            Bound {
                plan,
                scope: Scope { cols: cols.clone() },
            },
            limit,
            offset,
        )?;
        if bound.plan.width() > visible {
            bound.plan = Plan::Project {
                input: Box::new(bound.plan),
                exprs: (0..visible).map(Expr::Column).collect(),
            };
        }
        bound.scope = Scope { cols };
        Ok(bound)
    }

    /// `GROUP BY` items that are positions or aliases of the select list.
    fn group_item<'e>(&self, e: &'e ast::Expr, items: &'e [SelectItem]) -> Result<&'e ast::Expr> {
        let exprs: Vec<(&ast::Expr, Option<&String>)> = items
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { expr, alias } => Some((expr, alias.as_ref())),
                _ => None,
            })
            .collect();
        Ok(match e {
            ast::Expr::Literal(Value::Int(n)) => {
                let n = *n;
                if n < 1 || n as usize > exprs.len() {
                    return Err(Error::undefined_column(format!(
                        "GROUP BY position {n} is not in select list"
                    )));
                }
                exprs[n as usize - 1].0
            }
            ast::Expr::Column { table: None, name } => exprs
                .iter()
                .find(|(_, a)| a.is_some_and(|a| a == name))
                .map_or(e, |(x, _)| x),
            _ => e,
        })
    }

    fn grouped_column(&mut self, g: &mut Grouping, i: usize) -> Result<Expr> {
        match g.group.iter().position(|x| *x == Expr::Column(i)) {
            Some(gi) => Ok(Expr::Column(gi)),
            None => Err(Error::grouping(format!(
                "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                g.from.cols[i].name
            ))),
        }
    }

    /// Binds an expression over a grouped input: aggregate calls become
    /// aggregate outputs, grouped expressions group outputs, and any other
    /// column reference is an error.
    fn grouped_expr(&mut self, e: &ast::Expr, g: &mut Grouping) -> Result<Typed> {
        use ast::Expr as A;
        if let A::Function {
            name,
            args,
            distinct,
            star,
        } = e
            && let Some(func) = AggFunc::lookup(name)
        {
            let from = g.from.clone();
            let (arg, at) = match (star, args.as_slice()) {
                (true, []) if func == AggFunc::Count => (None, None),
                (false, [a]) => {
                    if contains_agg(a) {
                        return Err(Error::grouping("aggregate function calls cannot be nested"));
                    }
                    let (x, t) = self.expr(a, &from)?;
                    (Some(x), t)
                }
                _ => {
                    return Err(Error::undefined_function(format!(
                        "function {name} takes one argument"
                    )));
                }
            };
            let ty = match func {
                AggFunc::Count => Some(DataType::Int),
                AggFunc::Avg => Some(DataType::Float),
                AggFunc::Sum | AggFunc::Min | AggFunc::Max => at,
            };
            if matches!(func, AggFunc::Sum | AggFunc::Avg)
                && at.is_some_and(|t| !matches!(t, DataType::Int | DataType::Float))
            {
                return Err(Error::undefined_function(format!(
                    "function {name}({}) does not exist",
                    at.unwrap()
                )));
            }
            let agg = Agg {
                func,
                arg,
                distinct: *distinct,
            };
            let slot = match g.aggs.iter().position(|a| *a == agg) {
                Some(j) => j,
                None => {
                    g.aggs.push(agg);
                    g.agg_types.push(ty);
                    g.aggs.len() - 1
                }
            };
            return Ok((Expr::Column(g.group.len() + slot), ty));
        }
        if !contains_agg(e) {
            let from = g.from.clone();
            let (x, t) = self.expr(e, &from)?;
            if let Some(gi) = g.group.iter().position(|y| *y == x) {
                return Ok((Expr::Column(gi), g.group_types[gi]));
            }
            let cols = x.columns();
            if cols.is_empty() {
                return Ok((x, t)); // constants, parameters, outer references
            }
            if let A::Column { .. } = e {
                return self.grouped_column(g, cols[0]).map(|x| (x, t));
            }
        }
        // Rebuild from grouped parts.
        let mut sub = |p: &ast::Expr, me: &mut Self| me.grouped_expr(p, g);
        self.expr_with(e, &mut sub)
    }

    // ----------------------------------------------------------- expressions

    /// Binds a boolean condition.
    pub fn condition(&mut self, e: &ast::Expr, scope: &Scope) -> Result<Expr> {
        let (x, t) = self.expr(e, scope)?;
        if let Expr::Param(n) = x {
            self.note_param(n, Some(DataType::Bool));
        } else if t.is_some_and(|t| t != DataType::Bool) {
            return Err(Error::type_mismatch(format!(
                "argument of WHERE must be type boolean, not type {}",
                t.unwrap()
            )));
        }
        Ok(x)
    }

    /// Binds an expression against `scope` (and the enclosing scopes).
    pub fn expr(&mut self, e: &ast::Expr, scope: &Scope) -> Result<Typed> {
        let mut sub = |p: &ast::Expr, me: &mut Self| me.expr(p, scope);
        if let ast::Expr::Column { table, name } = e {
            return self.column(table.as_deref(), name, scope);
        }
        if let ast::Expr::Function { name, .. } = e
            && is_aggregate(name)
        {
            return Err(Error::grouping(format!(
                "aggregate function {name} is not allowed here"
            )));
        }
        if let Some(r) = self.subquery_expr(e, scope)? {
            return Ok(r);
        }
        self.expr_with(e, &mut sub)
    }

    fn column(&self, table: Option<&str>, name: &str, scope: &Scope) -> Result<Typed> {
        if let Some(i) = scope.find(table, name)? {
            return Ok((Expr::Column(i), scope.cols[i].ty));
        }
        for (d, s) in self.outer.iter().rev().enumerate() {
            if let Some(i) = s.find(table, name)? {
                return Ok((
                    Expr::Outer {
                        depth: d + 1,
                        index: i,
                    },
                    s.cols[i].ty,
                ));
            }
        }
        if let Some(t) = table {
            if !scope.has_table(t) && !self.outer.iter().any(|s| s.has_table(t)) {
                return Err(Error::undefined_table(format!(
                    "missing FROM-clause entry for table \"{t}\""
                )));
            }
            return Err(Error::undefined_column(format!(
                "column {t}.{name} does not exist"
            )));
        }
        Err(Error::undefined_column(format!(
            "column \"{name}\" does not exist"
        )))
    }

    fn subquery(&mut self, q: &ast::Query, scope: &Scope) -> Result<(Plan, Scope, bool)> {
        self.outer.push(scope.clone());
        let r = self.query(q);
        self.outer.pop();
        let b = r?;
        let correlated = b.plan.outer_depth() > 0;
        Ok((
            crate::optimizer::optimize(b.plan, self.txn),
            b.scope,
            correlated,
        ))
    }

    fn subquery_expr(&mut self, e: &ast::Expr, scope: &Scope) -> Result<Option<Typed>> {
        use ast::Expr as A;
        Ok(Some(match e {
            A::Exists { query, negated } => {
                let (plan, _, correlated) = self.subquery(query, scope)?;
                let x = Expr::Subquery(Box::new(SubPlan {
                    kind: SubKind::Exists,
                    plan,
                    correlated,
                }));
                (
                    if *negated {
                        Expr::Unary(UnaryOp::Not, Box::new(x))
                    } else {
                        x
                    },
                    Some(DataType::Bool),
                )
            }
            A::Subquery(query) => {
                let (plan, s, correlated) = self.subquery(query, scope)?;
                if s.cols.len() != 1 {
                    return Err(Error::syntax("subquery must return only one column"));
                }
                (
                    Expr::Subquery(Box::new(SubPlan {
                        kind: SubKind::Scalar,
                        plan,
                        correlated,
                    })),
                    s.cols[0].ty,
                )
            }
            A::InSubquery {
                expr,
                query,
                negated,
            } => {
                let (x, xt) = self.expr(expr, scope)?;
                let (plan, s, correlated) = self.subquery(query, scope)?;
                if s.cols.len() != 1 {
                    return Err(Error::syntax("subquery has too many columns"));
                }
                comparable(xt, s.cols[0].ty)?;
                (
                    Expr::Subquery(Box::new(SubPlan {
                        kind: SubKind::In(Box::new(x), *negated),
                        plan,
                        correlated,
                    })),
                    Some(DataType::Bool),
                )
            }
            _ => return Ok(None),
        }))
    }

    /// The structural part of binding: children through `sub`.
    fn expr_with(
        &mut self,
        e: &ast::Expr,
        sub: &mut dyn FnMut(&ast::Expr, &mut Self) -> Result<Typed>,
    ) -> Result<Typed> {
        use ast::Expr as A;
        Ok(match e {
            A::Literal(v) => (Expr::Literal(v.clone()), v.data_type()),
            A::Column { .. } | A::Exists { .. } | A::Subquery(_) | A::InSubquery { .. } => {
                sub(e, self)?
            }
            A::Param(n) => {
                self.note_param(n - 1, None);
                (Expr::Param(n - 1), self.params[n - 1])
            }
            A::Unary { op, expr } => {
                let (x, t) = sub(expr, self)?;
                match op {
                    UnaryOp::Not => {
                        expect(t, DataType::Bool, "NOT")?;
                        (Expr::Unary(UnaryOp::Not, Box::new(x)), Some(DataType::Bool))
                    }
                    _ => {
                        if t.is_some_and(|t| !matches!(t, DataType::Int | DataType::Float)) {
                            return Err(Error::undefined_function(format!(
                                "operator does not exist: -{}",
                                t.unwrap()
                            )));
                        }
                        (Expr::Unary(*op, Box::new(x)), t)
                    }
                }
            }
            A::Binary { op, left, right } => {
                let (l, lt) = sub(left, self)?;
                let (r, rt) = sub(right, self)?;
                self.binary(*op, l, lt, r, rt)?
            }
            A::IsNull { expr, negated } => {
                let (x, _) = sub(expr, self)?;
                (Expr::IsNull(Box::new(x), *negated), Some(DataType::Bool))
            }
            A::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let (x, xt) = sub(expr, self)?;
                let (lo, lot) = sub(low, self)?;
                let (hi, hit) = sub(high, self)?;
                let (ge, _) = self.binary(BinaryOp::GtEq, x.clone(), xt, lo, lot)?;
                let (le, _) = self.binary(BinaryOp::LtEq, x, xt, hi, hit)?;
                let both = Expr::Binary(BinaryOp::And, Box::new(ge), Box::new(le));
                (
                    if *negated {
                        Expr::Unary(UnaryOp::Not, Box::new(both))
                    } else {
                        both
                    },
                    Some(DataType::Bool),
                )
            }
            A::InList {
                expr,
                list,
                negated,
            } => {
                let (x, xt) = sub(expr, self)?;
                let mut items = Vec::new();
                for item in list {
                    let (i, it) = sub(item, self)?;
                    let (_, i) = self.coerce(xt, i, it)?;
                    comparable(xt, i.1)?;
                    items.push(i.0);
                }
                (
                    Expr::InList(Box::new(x), items, *negated),
                    Some(DataType::Bool),
                )
            }
            A::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let (x, xt) = sub(expr, self)?;
                let (p, pt) = sub(pattern, self)?;
                let (x, _) = self.text_operand(x, xt, "LIKE")?;
                let (p, _) = self.text_operand(p, pt, "LIKE")?;
                (
                    Expr::Like {
                        expr: Box::new(x),
                        pattern: Box::new(p),
                        negated: *negated,
                        case_insensitive: *case_insensitive,
                    },
                    Some(DataType::Bool),
                )
            }
            A::Case {
                operand,
                whens,
                otherwise,
            } => {
                let op = operand.as_ref().map(|o| sub(o, self)).transpose()?;
                let mut ty = None;
                let mut bound = Vec::new();
                for (w, t) in whens {
                    let (wx, wt) = sub(w, self)?;
                    match &op {
                        Some((_, ot)) => comparable(*ot, wt)?,
                        None => expect(wt, DataType::Bool, "CASE WHEN")?,
                    }
                    let (tx, tt) = sub(t, self)?;
                    ty = unify(ty, tt)?;
                    bound.push((wx, tx));
                }
                let other = otherwise.as_ref().map(|o| sub(o, self)).transpose()?;
                if let Some((_, ot)) = &other {
                    ty = unify(ty, *ot)?;
                }
                let cast = |x: Expr, t: Option<DataType>| {
                    if ty == Some(DataType::Float) && t == Some(DataType::Int) {
                        Expr::Cast(Box::new(x), DataType::Float)
                    } else {
                        x
                    }
                };
                (
                    Expr::Case {
                        operand: op.map(|(x, _)| Box::new(x)),
                        whens: bound.into_iter().map(|(w, t)| (w, cast(t, None))).collect(),
                        otherwise: other.map(|(x, t)| Box::new(cast(x, t))),
                    },
                    ty,
                )
            }
            A::Cast { expr, ty } => {
                let (x, _) = sub(expr, self)?;
                if let Expr::Param(n) = x {
                    self.note_param(n, Some(*ty));
                }
                (Expr::Cast(Box::new(x), *ty), Some(*ty))
            }
            A::Function {
                name,
                args,
                distinct,
                star,
            } => {
                if *star || *distinct {
                    return Err(Error::syntax(format!(
                        "{name}(*) / DISTINCT is only for aggregates"
                    )));
                }
                let (f, arity) = Func::lookup(name).ok_or_else(|| {
                    Error::undefined_function(format!("function {name} does not exist"))
                })?;
                if !arity.contains(&args.len()) {
                    return Err(Error::undefined_function(format!(
                        "function {name} with {} arguments does not exist",
                        args.len()
                    )));
                }
                let mut bound = Vec::new();
                let mut types = Vec::new();
                for a in args {
                    let (x, t) = sub(a, self)?;
                    bound.push(x);
                    types.push(t);
                }
                let ty = match f {
                    Func::Lower
                    | Func::Upper
                    | Func::Trim
                    | Func::Substr
                    | Func::Concat
                    | Func::Replace
                    | Func::Version => Some(DataType::Text),
                    Func::Length | Func::Strpos => Some(DataType::Int),
                    Func::IsDistinctFrom => Some(DataType::Bool),
                    Func::Sqrt | Func::Power => Some(DataType::Float),
                    Func::Abs | Func::Round | Func::Floor | Func::Ceil | Func::Nullif => types[0],
                    Func::Coalesce | Func::Greatest | Func::Least => {
                        let mut t = None;
                        for x in &types {
                            t = unify(t, *x)?;
                        }
                        if t == Some(DataType::Float) {
                            for (x, xt) in bound.iter_mut().zip(&types) {
                                if *xt == Some(DataType::Int) {
                                    *x = Expr::Cast(Box::new(x.clone()), DataType::Float);
                                }
                            }
                        }
                        t
                    }
                };
                (Expr::Func(f, bound), ty)
            }
        })
    }

    fn text_operand(&mut self, x: Expr, t: Option<DataType>, what: &str) -> Result<Typed> {
        if let Expr::Param(n) = x {
            self.note_param(n, Some(DataType::Text));
            return Ok((x, Some(DataType::Text)));
        }
        if t.is_some_and(|t| t != DataType::Text) {
            return Err(Error::undefined_function(format!(
                "operator does not exist: {} {what} text",
                t.unwrap()
            )));
        }
        Ok((x, Some(DataType::Text)))
    }

    /// Gives an untyped side (a text literal, `NULL`, a parameter) the type
    /// of the other side.
    fn coerce(
        &mut self,
        target: Option<DataType>,
        x: Expr,
        t: Option<DataType>,
    ) -> Result<(Expr, Typed)> {
        let Some(target) = target else {
            return Ok((x.clone(), (x, t)));
        };
        Ok(match &x {
            Expr::Param(n) => {
                self.note_param(*n, Some(target));
                (x.clone(), (x, Some(target)))
            }
            Expr::Literal(Value::Text(_)) if target != DataType::Text => {
                let v = match &x {
                    Expr::Literal(v) => v.clone().cast(target)?,
                    _ => unreachable!(),
                };
                let e = Expr::Literal(v);
                (e.clone(), (e, Some(target)))
            }
            _ => (x.clone(), (x, t)),
        })
    }

    fn binary(
        &mut self,
        op: BinaryOp,
        l: Expr,
        lt: Option<DataType>,
        r: Expr,
        rt: Option<DataType>,
    ) -> Result<Typed> {
        use BinaryOp::*;
        // An untyped side takes the other side's type — for comparisons and
        // arithmetic.  `||` yields text whatever its operands are, so
        // `'n' || 3` keeps 'n' as text (as PostgreSQL does).
        let (l, lt, r, rt) = if op == Concat {
            (l, lt, r, rt)
        } else {
            let (l, (_, lt)) = self.coerce(rt, l, lt)?;
            let (r, (_, rt)) = self.coerce(lt, r, rt)?;
            (l, lt, r, rt)
        };
        let ty = match op {
            And | Or => {
                expect(lt, DataType::Bool, op.symbol())?;
                expect(rt, DataType::Bool, op.symbol())?;
                Some(DataType::Bool)
            }
            Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                comparable(lt, rt)?;
                Some(DataType::Bool)
            }
            Concat => {
                if lt == Some(DataType::Text)
                    || rt == Some(DataType::Text)
                    || (lt.is_none() && rt.is_none())
                {
                    Some(DataType::Text)
                } else {
                    return Err(Error::undefined_function(format!(
                        "operator does not exist: {} || {}",
                        name(lt),
                        name(rt)
                    )));
                }
            }
            Add | Sub | Mul | Div | Mod => match (lt, rt) {
                (Some(DataType::Int), Some(DataType::Int)) => Some(DataType::Int),
                (
                    Some(DataType::Int | DataType::Float) | None,
                    Some(DataType::Int | DataType::Float) | None,
                ) => {
                    if lt == Some(DataType::Float) || rt == Some(DataType::Float) {
                        Some(DataType::Float)
                    } else {
                        lt.or(rt)
                    }
                }
                _ => {
                    return Err(Error::undefined_function(format!(
                        "operator does not exist: {} {} {}",
                        name(lt),
                        op.symbol(),
                        name(rt)
                    )));
                }
            },
        };
        Ok((Expr::Binary(op, Box::new(l), Box::new(r)), ty))
    }
}

/// Applies a `FROM` item's column alias list.
fn rename_columns(scope: &mut Scope, alias: &str, names: &[String]) -> Result<()> {
    if names.len() > scope.cols.len() {
        return Err(Error::invalid_column_reference(format!(
            "table \"{alias}\" has {} columns available but {} columns specified",
            scope.cols.len(),
            names.len()
        )));
    }
    for (c, n) in scope.cols.iter_mut().zip(names) {
        c.name = n.clone();
    }
    Ok(())
}

fn name(t: Option<DataType>) -> &'static str {
    t.map_or("unknown", DataType::name)
}

fn expect(t: Option<DataType>, want: DataType, what: &str) -> Result<()> {
    match t {
        Some(t) if t != want => Err(Error::type_mismatch(format!(
            "argument of {what} must be type {want}, not type {t}"
        ))),
        _ => Ok(()),
    }
}

/// Whether two types can be compared.
fn comparable(a: Option<DataType>, b: Option<DataType>) -> Result<()> {
    use DataType::*;
    match (a, b) {
        (None, _) | (_, None) => Ok(()),
        (Some(x), Some(y)) if x == y => Ok(()),
        (Some(Int | Float), Some(Int | Float)) => Ok(()),
        (Some(x), Some(y)) => Err(Error::undefined_function(format!(
            "operator does not exist: {x} = {y}"
        ))),
    }
}

/// The common type of two branches (`CASE`, `UNION`, `VALUES`).
fn unify(a: Option<DataType>, b: Option<DataType>) -> Result<Option<DataType>> {
    use DataType::*;
    Ok(match (a, b) {
        (None, t) | (t, None) => t,
        (Some(x), Some(y)) if x == y => Some(x),
        (Some(Int | Float), Some(Int | Float)) => Some(Float),
        (Some(x), Some(y)) => {
            return Err(Error::type_mismatch(format!(
                "types {x} and {y} cannot be matched"
            )));
        }
    })
}
