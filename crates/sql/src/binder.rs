//! The binder: syntax tree → plan, with names resolved, types inferred and
//! aggregates lifted out.

use nucleus_kv::KvTxn;

use crate::ast::{self, BinaryOp, JoinKind, SelectItem, SetExpr, TableRef, UnaryOp};
use crate::builtin;
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::expr::{Expr, Func, SubKind, SubPlan};
use crate::ext::{Arg, ArgInfo, FnRef, Registry, Returns};
use crate::plan::{Access, Agg, JoinAlgo, JoinType, Plan, SortKey, TableFunc};
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
    registry: &'a Registry,
    /// Enclosing queries' scopes, innermost last.
    outer: Vec<Scope>,
    /// Inferred parameter types, by `$n - 1`.
    pub params: Vec<Option<DataType>>,
    /// The CTEs in scope, innermost last.
    ctes: Vec<CteDef>,
}

/// A CTE visible to the query being bound.
#[derive(Clone)]
struct CteDef {
    name: String,
    /// Its columns.
    scope: Scope,
    /// A reference reads this plan, or (inside its own recursive term) the
    /// work table.
    plan: Option<Plan>,
    /// The work table id while its recursive term is being bound.
    work: usize,
    /// `outer.len()` where it was defined: a recursive reference must be
    /// at that level, not in a subquery.
    level: usize,
    /// Whether the recursive term referred to the work table.
    used: bool,
}

/// CTE and work table ids, unique in the process.
static NEXT_CTE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

fn next_cte_id() -> usize {
    NEXT_CTE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether `plan` reads a work table of a recursive CTE it is not itself.
fn reads_work_table(plan: &Plan) -> bool {
    fn open(plan: &Plan, closed: &mut Vec<usize>) -> bool {
        match plan {
            Plan::WorkTable { id, .. } => !closed.contains(id),
            Plan::Recursive { id, base, step, .. } => {
                closed.push(*id);
                let r = open(base, closed) || open(step, closed);
                closed.pop();
                r
            }
            p => p.children().into_iter().any(|c| open(c, closed)),
        }
    }
    open(plan, &mut Vec::new())
}

type Typed = (Expr, Option<DataType>);

/// Whether `e` calls an aggregate of `r` (not looking into subqueries).
fn contains_agg(r: &Registry, e: &ast::Expr) -> bool {
    use ast::Expr as A;
    let contains_agg = |e: &ast::Expr| contains_agg(r, e);
    match e {
        A::Function { name, args, .. } => {
            r.aggregate(name).is_some() || args.iter().any(&contains_agg)
        }
        A::Unary { expr, .. } | A::IsNull { expr, .. } | A::Cast { expr, .. } => contains_agg(expr),
        A::Binary { left, right, .. } => contains_agg(left) || contains_agg(right),
        A::Between {
            expr, low, high, ..
        } => contains_agg(expr) || contains_agg(low) || contains_agg(high),
        A::InList { expr, list, .. } => contains_agg(expr) || list.iter().any(&contains_agg),
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
        A::Array(items) => items.iter().any(&contains_agg),
        A::Subscript { expr, index } => contains_agg(expr) || contains_agg(index),
        A::AnyAll { left, right, .. } => contains_agg(left) || contains_agg(right),
        A::Literal(_)
        | A::Column { .. }
        | A::Param(_)
        | A::Exists { .. }
        | A::Subquery(_)
        | A::ArraySubquery(_) => false,
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
        ast::Expr::Array(_) | ast::Expr::ArraySubquery(_) => "array".into(),
        ast::Expr::Subscript { expr, .. } => default_name(expr),
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
    pub fn new(txn: &'a dyn KvTxn, registry: &'a Registry) -> Self {
        Self {
            txn,
            registry,
            outer: Vec::new(),
            params: Vec::new(),
            ctes: Vec::new(),
        }
    }

    /// The registry functions and views are found in.
    pub fn registry(&self) -> &'a Registry {
        self.registry
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
        let mark = self.ctes.len();
        let r = self.with_and_body(q);
        self.ctes.truncate(mark);
        r
    }

    fn with_and_body(&mut self, q: &ast::Query) -> Result<Bound> {
        if let Some(w) = &q.with {
            for (i, cte) in w.ctes.iter().enumerate() {
                if w.ctes[..i].iter().any(|c| c.name == cte.name) {
                    return Err(Error::duplicate_alias(format!(
                        "WITH query name \"{}\" specified more than once",
                        cte.name
                    )));
                }
                let def = if w.recursive {
                    self.recursive_cte(cte)?
                } else {
                    self.plain_cte(cte)?
                };
                self.ctes.push(def);
            }
        }
        self.query_body(q)
    }

    fn cte_def(&self, cte: &ast::Cte, mut scope: Scope, plan: Plan) -> Result<CteDef> {
        if plan.outer_depth() > 0 {
            return Err(Error::feature(
                "a WITH query referring to columns of an enclosing query is not supported",
            ));
        }
        for c in &mut scope.cols {
            c.table = None;
        }
        rename_columns(&mut scope, &cte.name, &cte.columns)?;
        let plan = crate::optimizer::optimize(plan, self.txn);
        // Computed once and shared — unless it reads a work table, whose
        // rows change from round to round.
        let plan = if reads_work_table(&plan) {
            plan
        } else {
            Plan::Cte {
                id: next_cte_id(),
                name: cte.name.clone(),
                plan: Box::new(plan),
            }
        };
        Ok(CteDef {
            name: cte.name.clone(),
            scope,
            plan: Some(plan),
            work: 0,
            level: self.outer.len(),
            used: false,
        })
    }

    fn plain_cte(&mut self, cte: &ast::Cte) -> Result<CteDef> {
        let b = self.query(&cte.query)?;
        self.cte_def(cte, b.scope, b.plan)
    }

    /// `name AS (base UNION [ALL] step)` where `step` may read `name`.
    fn recursive_cte(&mut self, cte: &ast::Cte) -> Result<CteDef> {
        let q = &cte.query;
        let SetExpr::SetOp {
            op: ast::SetOp::Union,
            left,
            right,
            all,
        } = &q.body
        else {
            return self.plain_cte(cte);
        };
        if q.with.is_some() || !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
            return self.plain_cte(cte);
        }
        let base = self.set_expr(left)?;
        let mut scope = base.scope.clone();
        for c in &mut scope.cols {
            c.table = None;
        }
        rename_columns(&mut scope, &cte.name, &cte.columns)?;
        let work = next_cte_id();
        self.ctes.push(CteDef {
            name: cte.name.clone(),
            scope: scope.clone(),
            plan: None,
            work,
            level: self.outer.len(),
            used: false,
        });
        let at = self.ctes.len() - 1;
        let step = self.set_expr(right);
        let used = self.ctes[at].used;
        self.ctes.truncate(at);
        let step = step?;
        if step.scope.cols.len() != base.scope.cols.len() {
            return Err(Error::syntax(
                "each UNION query must have the same number of columns",
            ));
        }
        for (i, (b, s)) in base.scope.cols.iter().zip(&step.scope.cols).enumerate() {
            if unify(b.ty, s.ty)? != b.ty && b.ty.is_some() {
                return Err(Error::type_mismatch(format!(
                    "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                    cte.name,
                    i + 1,
                    name(b.ty),
                    name(unify(b.ty, s.ty)?)
                )));
            }
        }
        let plan = if used {
            Plan::Recursive {
                id: work,
                base: Box::new(crate::optimizer::optimize(base.plan, self.txn)),
                step: Box::new(crate::optimizer::optimize(step.plan, self.txn)),
                all: *all,
            }
        } else {
            Plan::SetOp {
                op: ast::SetOp::Union,
                left: Box::new(base.plan),
                right: Box::new(step.plan),
                all: *all,
            }
        };
        self.cte_def(cte, base.scope, plan)
    }

    /// A function in `FROM`.
    fn function_ref(
        &mut self,
        name: &str,
        args: &[ast::Expr],
        ordinality: bool,
        alias: Option<&str>,
        columns: &[String],
    ) -> Result<Bound> {
        let empty = Scope::default();
        let (func, bound, ty) = match self.registry.table_function(name).cloned() {
            Some(f) => {
                if !f.arity().contains(&args.len()) {
                    return Err(Error::undefined_function(format!(
                        "function {name} with {} arguments does not exist",
                        args.len()
                    )));
                }
                let mut typed = Vec::new();
                for a in args {
                    typed.push(self.expr(a, &empty)?);
                }
                let sig = f.resolve(&arg_info(&typed))?;
                let [(_, ty)] = sig.columns.as_slice() else {
                    return Err(Error::feature(format!(
                        "table function {name} returns {} columns; one is supported",
                        sig.columns.len()
                    )));
                };
                let ty = *ty;
                let (bound, _) = self.apply_signature(name, sig.args, typed)?;
                (TableFunc::Call(f), bound, ty)
            }
            None => {
                // A scalar function: one row.
                let call = ast::Expr::Function {
                    name: name.to_string(),
                    args: args.to_vec(),
                    distinct: false,
                    star: false,
                    order_by: vec![],
                };
                let (x, t) = self.expr(&call, &empty)?;
                (TableFunc::Scalar, vec![x], t)
            }
        };
        let qual = alias.unwrap_or(name).to_string();
        let mut cols = vec![ScopeCol {
            table: Some(qual.clone()),
            name: qual.clone(),
            ty,
            hidden: false,
        }];
        if ordinality {
            cols.push(ScopeCol {
                table: Some(qual.clone()),
                name: "ordinality".into(),
                ty: Some(DataType::Int),
                hidden: false,
            });
        }
        let mut scope = Scope { cols };
        rename_columns(&mut scope, &qual, columns)?;
        Ok(Bound {
            plan: Plan::Function {
                func,
                args: bound,
                width: scope.cols.len(),
                ordinality,
            },
            scope,
        })
    }

    /// A reference to CTE `name`, if one is in scope.
    fn cte_ref(&mut self, name: &str, qual: &str, columns: &[String]) -> Result<Option<Bound>> {
        let Some(i) = self.ctes.iter().rposition(|c| c.name == name) else {
            return Ok(None);
        };
        let def = &self.ctes[i];
        let mut scope = def.scope.clone();
        for c in &mut scope.cols {
            c.table = Some(qual.to_string());
        }
        rename_columns(&mut scope, qual, columns)?;
        let plan = match &def.plan {
            Some(p) => p.clone(),
            None => {
                if self.outer.len() != def.level {
                    return Err(Error::invalid_recursion(format!(
                        "recursive reference to query \"{name}\" must not appear within a subquery"
                    )));
                }
                let p = Plan::WorkTable {
                    id: def.work,
                    name: name.to_string(),
                    width: def.scope.cols.len(),
                };
                self.ctes[i].used = true;
                p
            }
        };
        Ok(Some(Bound { plan, scope }))
    }

    fn query_body(&mut self, q: &ast::Query) -> Result<Bound> {
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
                require(bound.scope.cols[i].ty, &[Need::Ordering], "ORDER BY")?;
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
                    if !*all || *op != ast::SetOp::Union {
                        require(c.ty, &[Need::Equality, Need::Hashing], op.keyword())?;
                    }
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
        let empty = Scope::default();
        let mut cells: Vec<Vec<Typed>> = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != width {
                return Err(Error::syntax("VALUES lists must all be the same length"));
            }
            cells.push(
                row.iter()
                    .map(|e| self.expr(e, &empty))
                    .collect::<Result<_>>()?,
            );
        }
        // Each column takes the common type of its cells.
        let mut types: Vec<Option<DataType>> = vec![None; width];
        for (i, ty) in types.iter_mut().enumerate() {
            let mut column: Vec<Typed> = cells.iter_mut().map(|r| r[i].clone()).collect();
            *ty = self.unify_items(&mut column)?;
            for (r, c) in cells.iter_mut().zip(column) {
                r[i] = c;
            }
        }
        let out: Vec<Vec<Expr>> = cells
            .into_iter()
            .map(|r| r.into_iter().map(|(x, _)| x).collect())
            .collect();
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
            TableRef::Function {
                name,
                args,
                ordinality,
                alias,
                columns,
            } => self.function_ref(name, args, *ordinality, alias.as_deref(), columns),
            TableRef::Table {
                schema,
                name,
                alias,
                columns,
            } => {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                if schema.is_none()
                    && let Some(b) = self.cte_ref(name, &qual, columns)?
                {
                    return Ok(b);
                }
                // Views of searched schemas (pg_catalog, with pg-compat)
                // after the user's tables; others only by schema.
                let user = match schema.as_deref() {
                    None => catalog::get_table(self.txn, name)?,
                    Some("public") => Some(self.table(name)?),
                    Some(_) => None,
                };
                let table = match user {
                    Some(t) => t,
                    None => match self.registry.view(schema.as_deref(), name) {
                        Some(view) => {
                            let mut scope = Scope {
                                cols: view
                                    .columns()
                                    .into_iter()
                                    .map(|(name, ty)| ScopeCol {
                                        table: Some(qual.clone()),
                                        name,
                                        ty: Some(ty),
                                        hidden: false,
                                    })
                                    .collect(),
                            };
                            rename_columns(&mut scope, &qual, columns)?;
                            return Ok(Bound {
                                plan: Plan::View { view: view.clone() },
                                scope,
                            });
                        }
                        None => {
                            let full = match schema {
                                Some(s) => format!("{s}.{name}"),
                                None => name.clone(),
                            };
                            return Err(Error::undefined_table(format!(
                                "relation \"{full}\" does not exist"
                            )));
                        }
                    },
                };
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
                        require(lt.or(rt), &[Need::Equality], "JOIN USING")?;
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
                        let side = |i: usize| convert(Expr::Column(i), b.scope.cols[i].ty, *ty);
                        exprs.push(Expr::Call(
                            builtin::func(Func::Coalesce),
                            vec![side(*li), side(*ri)],
                        ));
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
            if contains_agg(self.registry, w) {
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
            || s.items.iter().any(
                |i| matches!(i, SelectItem::Expr { expr, .. } if contains_agg(self.registry, expr)),
            )
            || order_by
                .iter()
                .any(|o| contains_agg(self.registry, &o.expr));
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
                require(t, &[Need::Equality, Need::Hashing], "GROUP BY")?;
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
                require(cols[i].ty, &[Need::Ordering], "ORDER BY")?;
                keys.push(SortKey {
                    expr: Expr::Column(i),
                    desc: item.desc,
                    nulls_first,
                });
                continue;
            }
            let (x, t) = match &mut grouping {
                Some(g) => self.grouped_expr(&item.expr, g)?,
                None => self.expr(&item.expr, &scope)?,
            };
            require(t, &[Need::Ordering], "ORDER BY")?;
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
            for c in &cols {
                require(c.ty, &[Need::Equality, Need::Hashing], "DISTINCT")?;
            }
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
            order_by,
        } = e
            && let Some(func) = self.registry.aggregate(name).cloned()
        {
            let from = g.from.clone();
            let mut order = Vec::new();
            for o in order_by {
                if contains_agg(self.registry, &o.expr) {
                    return Err(Error::grouping("aggregate function calls cannot be nested"));
                }
                let (x, t) = self.expr(&o.expr, &from)?;
                require(t, &[Need::Ordering], "ORDER BY")?;
                order.push(SortKey {
                    expr: x,
                    desc: o.desc,
                    nulls_first: o.nulls_first.unwrap_or(o.desc),
                });
            }
            let shape_ok = if *star {
                args.is_empty() && func.accepts_star()
            } else {
                func.arity().contains(&args.len())
            };
            if !shape_ok {
                return Err(Error::undefined_function(format!(
                    "function {name} with {} arguments does not exist",
                    args.len()
                )));
            }
            let mut bound = Vec::new();
            for a in args {
                if contains_agg(self.registry, a) {
                    return Err(Error::grouping("aggregate function calls cannot be nested"));
                }
                let (x, t) = self.expr(a, &from)?;
                if *distinct {
                    require(t, &[Need::Equality, Need::Hashing], "DISTINCT")?;
                }
                bound.push((x, t));
            }
            let sig = func.resolve(&arg_info(&bound))?;
            let (args, common) = self.apply_signature(name, sig.args, bound)?;
            let ty = match sig.returns {
                Returns::Type(t) => t,
                Returns::Common => common,
                Returns::Constant(_) => {
                    return Err(Error::internal(format!(
                        "aggregate {name} resolved to a constant"
                    )));
                }
            };
            let agg = Agg {
                func,
                args,
                distinct: *distinct,
                order,
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
        if !contains_agg(self.registry, e) {
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
            && self.registry.aggregate(name).is_some()
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
            A::ArraySubquery(query) => {
                let (plan, s, correlated) = self.subquery(query, scope)?;
                if s.cols.len() != 1 {
                    return Err(Error::syntax("subquery must return only one column"));
                }
                let ty = s.cols[0]
                    .ty
                    .unwrap_or(DataType::Text)
                    .array_of()
                    .ok_or_else(|| Error::feature("arrays of arrays are not supported"))?;
                (
                    Expr::Subquery(Box::new(SubPlan {
                        kind: SubKind::Array,
                        plan,
                        correlated,
                    })),
                    Some(ty),
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
                require(xt.or(s.cols[0].ty), &[Need::Equality], "IN")?;
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
            A::Column { .. }
            | A::Exists { .. }
            | A::Subquery(_)
            | A::InSubquery { .. }
            | A::ArraySubquery(_) => sub(e, self)?,
            A::Array(items) => {
                let mut bound = Vec::new();
                for i in items {
                    bound.push(sub(i, self)?);
                }
                let ty = self.unify_items(&mut bound)?.unwrap_or(DataType::Text);
                let ty = ty
                    .array_of()
                    .ok_or_else(|| Error::feature("arrays of arrays are not supported"))?;
                (
                    Expr::Array(bound.into_iter().map(|(x, _)| x).collect()),
                    Some(ty),
                )
            }
            A::Subscript { expr, index } => {
                let (a, at) = sub(expr, self)?;
                let (i, it) = sub(index, self)?;
                let (i, (_, it)) = self.coerce(Some(DataType::Int), i, it)?;
                expect(it, DataType::Int, "an array subscript")?;
                if at.is_some_and(|t| t.element().is_none()) {
                    return Err(Error::type_mismatch(format!(
                        "cannot subscript type {} because it is not an array",
                        at.unwrap()
                    )));
                }
                (
                    Expr::Subscript(Box::new(a), Box::new(i)),
                    at.and_then(DataType::element),
                )
            }
            A::AnyAll {
                left,
                op,
                right,
                all,
            } => {
                let (l, lt) = sub(left, self)?;
                let (r, rt) = sub(right, self)?;
                // An untyped array takes the left side's array type.
                let want = lt.and_then(DataType::array_of);
                let (r, (_, rt)) = self.coerce(want, r, rt)?;
                if rt.is_some_and(|t| t.element().is_none()) {
                    return Err(Error::type_mismatch(format!(
                        "op ANY/ALL (array) requires array on right side, not {}",
                        rt.unwrap()
                    )));
                }
                let et = rt.and_then(DataType::element);
                let (l, (_, lt)) = self.coerce(et, l, lt)?;
                if !matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Lt
                        | BinaryOp::LtEq
                        | BinaryOp::Gt
                        | BinaryOp::GtEq
                        | BinaryOp::Match
                        | BinaryOp::NotMatch
                        | BinaryOp::IMatch
                        | BinaryOp::NotIMatch
                ) {
                    return Err(Error::type_mismatch(
                        "op ANY/ALL (array) requires an operator yielding boolean",
                    ));
                }
                comparable(lt, et)?;
                let need = if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                    Need::Equality
                } else {
                    Need::Ordering
                };
                require(lt.or(et), &[need], op.symbol())?;
                (
                    Expr::AnyAll {
                        left: Box::new(l),
                        op: *op,
                        right: Box::new(r),
                        all: *all,
                    },
                    Some(DataType::Bool),
                )
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
                        if t.is_some_and(|t| !t.is_number() && t != DataType::Interval) {
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
                    require(xt.or(i.1), &[Need::Equality], "IN")?;
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
                let mut conds = Vec::new();
                let mut results = Vec::new();
                for (w, t) in whens {
                    let (wx, wt) = sub(w, self)?;
                    let wx = match &op {
                        Some((_, ot)) => {
                            let (wx, (_, wt)) = self.coerce(*ot, wx, wt)?;
                            comparable(*ot, wt)?;
                            require(ot.or(wt), &[Need::Equality], "CASE")?;
                            wx
                        }
                        None => {
                            expect(wt, DataType::Bool, "CASE WHEN")?;
                            wx
                        }
                    };
                    conds.push(wx);
                    results.push(sub(t, self)?);
                }
                let has_else = otherwise.is_some();
                if let Some(o) = otherwise {
                    results.push(sub(o, self)?);
                }
                let ty = self.unify_items(&mut results)?;
                let other = if has_else {
                    results.pop().map(|(x, _)| Box::new(x))
                } else {
                    None
                };
                (
                    Expr::Case {
                        operand: op.map(|(x, _)| Box::new(x)),
                        whens: conds
                            .into_iter()
                            .zip(results.into_iter().map(|(x, _)| x))
                            .collect(),
                        otherwise: other,
                    },
                    ty,
                )
            }
            A::Cast { expr, ty, typmod } => {
                let Some(ty) = &resolve_type(self.registry, ty)? else {
                    // Not a type: a cast function (`::regclass`), or nothing.
                    let ast::SqlType::Named { name, .. } = ty else {
                        unreachable!("built-in types resolve")
                    };
                    let Some(f) = self.registry.function(&format!("__{name}")).cloned() else {
                        return Err(Error::undefined_object(format!(
                            "type {name} does not exist"
                        )));
                    };
                    let arg = sub(expr, self)?;
                    return self.call(f, vec![arg]);
                };
                let (x, _) = sub(expr, self)?;
                if let Expr::Param(n) = x {
                    self.note_param(n, Some(*ty));
                }
                let mut e = Expr::Cast(Box::new(x), *ty);
                let lit = |n: u32| Expr::Literal(Value::Int(i64::from(n)));
                if let (DataType::Numeric, Some((p, s))) = (ty, typmod.numeric) {
                    e = Expr::Call(builtin::func(Func::NumericFit), vec![e, lit(p), lit(s)]);
                }
                if let (DataType::Text, Some(n)) = (ty, typmod.max_len) {
                    // An explicit cast to varchar(n) truncates.
                    e = Expr::Call(builtin::func(Func::Substr), vec![e, lit(1), lit(n)]);
                }
                (e, Some(*ty))
            }
            A::Function {
                name,
                args,
                distinct,
                star,
                order_by,
            } => {
                if *star || *distinct || !order_by.is_empty() {
                    return Err(Error::syntax(format!(
                        "{name}(*), DISTINCT and ORDER BY are only for aggregates"
                    )));
                }
                let f = match self.registry.function(name) {
                    Some(f) => f.clone(),
                    None => {
                        return Err(Error::undefined_function(format!(
                            "function {name} does not exist"
                        )));
                    }
                };
                if !f.arity().contains(&args.len()) {
                    return Err(Error::undefined_function(format!(
                        "function {name} with {} arguments does not exist",
                        args.len()
                    )));
                }
                let mut bound = Vec::new();
                for a in args {
                    bound.push(sub(a, self)?);
                }
                self.call(f, bound)?
            }
        })
    }

    /// A call of `f` with bound arguments, typed by its signature.
    fn call(&mut self, f: FnRef, args: Vec<Typed>) -> Result<Typed> {
        let sig = f.resolve(&arg_info(&args))?;
        if let Returns::Constant(v) = sig.returns {
            let t = v.data_type();
            return Ok((Expr::Literal(v), t));
        }
        let (args, ty) = self.apply_signature(f.name(), sig.args, args)?;
        let ty = match sig.returns {
            Returns::Type(t) => t,
            Returns::Common => ty,
            Returns::Constant(_) => unreachable!("returned above"),
        };
        Ok((Expr::Call(f, args), ty))
    }

    /// Converts bound arguments as a signature says; returns them and the
    /// common type of the [`Arg::Common`] ones.
    fn apply_signature(
        &mut self,
        name: &str,
        sig: Vec<Arg>,
        mut args: Vec<Typed>,
    ) -> Result<(Vec<Expr>, Option<DataType>)> {
        if sig.len() != args.len() {
            return Err(Error::internal(format!(
                "{name} resolved {} arguments of {}",
                sig.len(),
                args.len()
            )));
        }
        for (a, arg) in args.iter_mut().zip(&sig) {
            let (x, t) = std::mem::replace(a, (Expr::Literal(Value::Null), None));
            *a = match *arg {
                Arg::Keep | Arg::Common => (x, t),
                Arg::Untyped(ty) => self.coerce(Some(ty), x, t)?.1,
                Arg::Convert(ty) => {
                    let (x, t) = self.coerce(Some(ty), x, t)?.1;
                    (convert(x, t, Some(ty)), Some(ty))
                }
            };
        }
        let common: Vec<usize> = (0..args.len()).filter(|&i| sig[i] == Arg::Common).collect();
        let mut common_ty = None;
        if !common.is_empty() {
            let mut items: Vec<Typed> = common
                .iter()
                .map(|&i| std::mem::replace(&mut args[i], (Expr::Literal(Value::Null), None)))
                .collect();
            common_ty = self.unify_items(&mut items)?;
            for (&i, item) in common.iter().zip(items) {
                args[i] = item;
            }
        }
        Ok((args.into_iter().map(|(x, _)| x).collect(), common_ty))
    }

    /// The common type of `items`, each converted to it.  Text literals and
    /// parameters are untyped here: they take the others' type (as
    /// PostgreSQL's `unknown`).
    fn unify_items(&mut self, items: &mut [Typed]) -> Result<Option<DataType>> {
        let mut ty = None;
        for (x, t) in items.iter() {
            if !is_text_literal(x) {
                ty = unify(ty, *t)?;
            }
        }
        if ty.is_none() && items.iter().any(|(x, _)| is_text_literal(x)) {
            ty = Some(DataType::Text);
        }
        for item in items.iter_mut() {
            let (x, t) = std::mem::replace(item, (Expr::Literal(Value::Null), None));
            let (x, (_, t2)) = self.coerce(ty, x, t)?;
            *item = (convert(x, t2, ty), ty);
        }
        Ok(ty)
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
        // `timestamp + '1 day'`: the untyped side is an interval; `date ±
        // $1`: a number of days.
        let untyped = |x: &Expr| matches!(x, Expr::Param(_)) || is_text_literal(x);
        let stamp =
            |t: Option<DataType>| matches!(t, Some(DataType::Timestamp | DataType::TimestampTz));
        let (l, lt, r, rt) = if op == Add && stamp(lt) && untyped(&r) {
            let (r, (_, rt)) = self.coerce(Some(DataType::Interval), r, rt)?;
            (l, lt, r, rt)
        } else if op == Add && stamp(rt) && untyped(&l) {
            let (l, (_, lt)) = self.coerce(Some(DataType::Interval), l, lt)?;
            (l, lt, r, rt)
        } else if matches!(op, Add | Sub)
            && lt == Some(DataType::Date)
            && matches!(r, Expr::Param(_))
        {
            let (r, (_, rt)) = self.coerce(Some(DataType::Int), r, rt)?;
            (l, lt, r, rt)
        } else if matches!(op, Mul | Div) && lt == Some(DataType::Interval) && untyped(&r) {
            // interval × number: an untyped number is a float.
            let (r, (_, rt)) = self.coerce(Some(DataType::Float), r, rt)?;
            (l, lt, r, rt)
        } else if op == Mul && rt == Some(DataType::Interval) && untyped(&l) {
            let (l, (_, lt)) = self.coerce(Some(DataType::Float), l, lt)?;
            (l, lt, r, rt)
        } else {
            (l, lt, r, rt)
        };
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
                let need = if matches!(op, Eq | NotEq) {
                    Need::Equality
                } else {
                    Need::Ordering
                };
                require(lt.or(rt), &[need], op.symbol())?;
                Some(DataType::Bool)
            }
            Match | NotMatch | IMatch | NotIMatch => {
                let (l2, _) = self.text_operand(l, lt, op.symbol())?;
                let (r2, _) = self.text_operand(r, rt, op.symbol())?;
                return Ok((
                    Expr::Binary(op, Box::new(l2), Box::new(r2)),
                    Some(DataType::Bool),
                ));
            }
            Concat
                if lt.is_some_and(|t| t.element().is_some())
                    || rt.is_some_and(|t| t.element().is_some()) =>
            {
                // array || array, array || element, element || array.
                let arr = lt.filter(|t| t.element().is_some()).or(rt);
                let et = arr.and_then(DataType::element);
                let (l, (_, lt2)) = self.coerce(if lt == arr { arr } else { et }, l, lt)?;
                let (r, (_, rt2)) = self.coerce(if rt == arr { arr } else { et }, r, rt)?;
                for t in [lt2, rt2] {
                    if t.is_some_and(|t| t != arr.expect("an array side") && Some(t) != et) {
                        return Err(Error::undefined_function(format!(
                            "operator does not exist: {} || {}",
                            name(lt2),
                            name(rt2)
                        )));
                    }
                }
                return Ok((Expr::Binary(op, Box::new(l), Box::new(r)), arr));
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
            Add | Sub | Mul | Div | Mod => arith_type(op, lt, rt).ok_or_else(|| {
                Error::undefined_function(format!(
                    "operator does not exist: {} {} {}",
                    name(lt),
                    op.symbol(),
                    name(rt)
                ))
            })?,
        };
        Ok((Expr::Binary(op, Box::new(l), Box::new(r)), ty))
    }
}

/// The result type of an arithmetic operator, if it applies to these
/// operand types (`None` operands: untyped).
pub(crate) fn arith_type(
    op: BinaryOp,
    lt: Option<DataType>,
    rt: Option<DataType>,
) -> Option<Option<DataType>> {
    use BinaryOp::*;
    use DataType::*;
    let num = |t: Option<DataType>| t.is_none_or(DataType::is_number);
    Some(match (op, lt, rt) {
        (_, l, r) if num(l) && num(r) => {
            if l == Some(Float) || r == Some(Float) {
                Some(Float)
            } else if l == Some(Numeric) || r == Some(Numeric) {
                Some(Numeric)
            } else {
                l.or(r)
            }
        }
        (Add, Some(Date), Some(Int))
        | (Add, Some(Int), Some(Date))
        | (Sub, Some(Date), Some(Int)) => Some(Date),
        (Sub, Some(Date), Some(Date)) => Some(Int),
        (Add, Some(Date | Timestamp), Some(Interval))
        | (Add, Some(Interval), Some(Date | Timestamp))
        | (Sub, Some(Date | Timestamp), Some(Interval)) => Some(Timestamp),
        (Add, Some(TimestampTz), Some(Interval))
        | (Add, Some(Interval), Some(TimestampTz))
        | (Sub, Some(TimestampTz), Some(Interval)) => Some(TimestampTz),
        (Sub, Some(a), Some(b)) if a.is_datetime() && b.is_datetime() => Some(Interval),
        (Add | Sub, Some(Interval), Some(Interval)) => Some(Interval),
        (Mul, Some(Interval), r) | (Mul, r, Some(Interval)) | (Div, Some(Interval), r)
            if num(r) =>
        {
            Some(Interval)
        }
        _ => return None,
    })
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

/// An ability a construct needs of a type (PLAN.md §M14.19, "Type
/// capabilities").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// `=`, `IN`, `DISTINCT`, grouping, set operations, `UNIQUE`.
    Equality,
    /// Hash joins and aggregation, `DISTINCT`, grouping, set operations.
    Hashing,
    /// `<` …, `ORDER BY`, `min` / `max`.
    Ordering,
    /// Primary keys and indexes.
    KeyEncoding,
}

impl Need {
    fn name(self) -> &'static str {
        match self {
            Need::Equality => "equality",
            Need::Hashing => "hashing",
            Need::Ordering => "ordering",
            Need::KeyEncoding => "key encoding",
        }
    }
}

/// Refuses `t` for `what` unless it has the abilities `needs`, naming the
/// one missing (and what the type offers instead).
pub fn require(t: Option<DataType>, needs: &[Need], what: &str) -> Result<()> {
    let Some(t) = t else { return Ok(()) };
    let ty = crate::builtin::type_of(t);
    for &need in needs {
        let has = match need {
            Need::Equality => ty.equality().is_some(),
            Need::Hashing => ty.hashing().is_some(),
            Need::Ordering => ty.ordering().is_some(),
            Need::KeyEncoding => ty.key_encoding().is_some(),
        };
        if has {
            continue;
        }
        let hint = match (need, ty.distance()) {
            (Need::Ordering, Some(d)) => {
                format!("; it has a distance: ORDER BY x {} value", d.operator())
            }
            _ => String::new(),
        };
        let message = format!(
            "type {} has no {}, which {what} needs{hint}",
            ty.name(),
            need.name()
        );
        return Err(if need == Need::KeyEncoding {
            Error::undefined_object(message)
        } else {
            Error::undefined_function(message)
        });
    }
    Ok(())
}

/// Whether two types can be compared.
fn comparable(a: Option<DataType>, b: Option<DataType>) -> Result<()> {
    match (a, b) {
        (None, _) | (_, None) => Ok(()),
        (Some(x), Some(y))
            if x == y
                || (x.is_number() && y.is_number())
                || (x.is_datetime() && y.is_datetime()) =>
        {
            Ok(())
        }
        (Some(x), Some(y)) => Err(Error::undefined_function(format!(
            "operator does not exist: {x} = {y}"
        ))),
    }
}

/// The common type of two branches (`CASE`, `UNION`, `VALUES`): numbers
/// widen to `numeric`, then `double precision`; dates to `timestamp`, then
/// `timestamptz`.
pub fn unify(a: Option<DataType>, b: Option<DataType>) -> Result<Option<DataType>> {
    use DataType::*;
    let rank = |t: DataType| match t {
        Int | Date => 0,
        Numeric | Timestamp => 1,
        Float | TimestampTz => 2,
        _ => 0,
    };
    Ok(match (a, b) {
        (None, t) | (t, None) => t,
        (Some(x), Some(y)) if x == y => Some(x),
        (Some(x), Some(y))
            if (x.is_number() && y.is_number()) || (x.is_datetime() && y.is_datetime()) =>
        {
            Some(if rank(x) >= rank(y) { x } else { y })
        }
        (Some(x), Some(y)) => {
            return Err(Error::type_mismatch(format!(
                "types {x} and {y} cannot be matched"
            )));
        }
    })
}

/// `x` (of type `from`) converted to `to` where they differ.
fn convert(x: Expr, from: Option<DataType>, to: Option<DataType>) -> Expr {
    match (from, to) {
        (Some(f), Some(t)) if f != t => Expr::Cast(Box::new(x), t),
        _ => x,
    }
}

/// The type `t` names in `registry` (`None`: a name that is no type).  A
/// schema before it must exist: `public`, or a registered one.
pub fn resolve_type(registry: &Registry, t: &ast::SqlType) -> Result<Option<DataType>> {
    let (schema, found) = match t {
        ast::SqlType::Builtin { schema, ty } => (schema, Some(*ty)),
        ast::SqlType::Named { schema, name } => (schema, registry.type_named(name)),
    };
    if let Some(s) = schema
        && s != "public"
        && registry.schema(s).is_none()
    {
        return Err(Error::invalid_schema(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    Ok(found)
}

/// [`resolve_type`] for a column: the name must be a type.
pub fn column_type(registry: &Registry, t: &ast::SqlType) -> Result<DataType> {
    resolve_type(registry, t)?.ok_or_else(|| {
        let ast::SqlType::Named { name, .. } = t else {
            unreachable!("built-in types resolve")
        };
        Error::undefined_object(format!("type \"{name}\" does not exist"))
    })
}

/// What a function's `resolve` learns of bound arguments.
fn arg_info(args: &[Typed]) -> Vec<ArgInfo> {
    args.iter()
        .map(|(x, t)| ArgInfo {
            ty: *t,
            text_literal: is_text_literal(x),
        })
        .collect()
}

fn is_text_literal(x: &Expr) -> bool {
    matches!(x, Expr::Literal(Value::Text(_)))
}
