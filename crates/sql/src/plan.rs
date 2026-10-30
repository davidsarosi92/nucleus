//! Query plans: the tree the binder builds, the optimizer rewrites and the
//! executor runs.  Each node's output is a row of `width()` values.

use std::fmt::Write;
use std::ops::Bound;

use crate::ast::SetOp;
use crate::catalog::Table;
use crate::expr::{Expr, SubKind};
use crate::ext::{AggRef, TableFnRef, ViewRef};

/// A plan node.
// Scan carries its table definition inline; plans are few and short-lived,
// so boxing it would only complicate every constructor.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// A table's rows (all columns), through an access path, filtered.
    Scan {
        /// The table.
        table: Table,
        /// How rows are found.
        access: Access,
        /// Rows not satisfying this are skipped.
        filter: Option<Expr>,
        /// Estimated output rows (optimizer).
        estimate: Option<f64>,
    },
    /// Literal rows, evaluated against the enclosing row (if any).
    Values {
        /// The rows.
        rows: Vec<Vec<Expr>>,
        /// Columns per row.
        width: usize,
    },
    /// The rows of a table that its row-level security policies let
    /// through: a barrier no predicate of the statement is evaluated
    /// below ([`crate::policies`]).
    Secure {
        /// The table's scan (its key range may come from the statement).
        input: Box<Plan>,
        /// The policies' condition.
        policy: Expr,
    },
    /// Rows of `input` satisfying `predicate`.
    Filter {
        /// Input.
        input: Box<Plan>,
        /// The condition.
        predicate: Expr,
    },
    /// A new row computed from each input row.
    Project {
        /// Input.
        input: Box<Plan>,
        /// One expression per output column.
        exprs: Vec<Expr>,
    },
    /// A join; output = left columns then right columns.
    Join {
        /// Left (outer) input.
        left: Box<Plan>,
        /// Right (inner) input.
        right: Box<Plan>,
        /// Inner or left outer.
        kind: JoinType,
        /// The condition, over the combined row.
        on: Option<Expr>,
        /// How.
        algo: JoinAlgo,
    },
    /// Grouping; output = group values then aggregate values.
    Aggregate {
        /// Input.
        input: Box<Plan>,
        /// `GROUP BY` expressions.
        group: Vec<Expr>,
        /// Aggregates.
        aggs: Vec<Agg>,
    },
    /// Rows in order.
    Sort {
        /// Input.
        input: Box<Plan>,
        /// Keys, most significant first.
        keys: Vec<SortKey>,
    },
    /// `LIMIT` / `OFFSET` (expressions evaluated once).
    Limit {
        /// Input.
        input: Box<Plan>,
        /// At most this many rows.
        limit: Option<Expr>,
        /// After skipping this many.
        offset: Option<Expr>,
    },
    /// Duplicates removed (first occurrence kept).
    Distinct {
        /// Input.
        input: Box<Plan>,
    },
    /// A common table expression: its rows are computed once per
    /// execution and shared by every reference (all of which carry the same
    /// `id` and plan).
    Cte {
        /// Unique per CTE.
        id: usize,
        /// Name, for `EXPLAIN`.
        name: String,
        /// The CTE's query.
        plan: Box<Plan>,
    },
    /// `WITH RECURSIVE`: `base`, then `step` run again and again over the
    /// rows the previous round produced (the work table `id`) until a round
    /// produces none.
    Recursive {
        /// The work table the step reads.
        id: usize,
        /// The non-recursive term.
        base: Box<Plan>,
        /// The recursive term.
        step: Box<Plan>,
        /// `UNION ALL` (else rows already produced are dropped).
        all: bool,
    },
    /// A function's rows: a table function (`generate_series`, `unnest`)
    /// or a scalar function (one row).
    Function {
        /// Which.
        func: TableFunc,
        /// Arguments (evaluated once per execution).
        args: Vec<Expr>,
        /// Output columns (with the ordinality column).
        width: usize,
        /// `WITH ORDINALITY`: a 1-based row number after the other columns.
        ordinality: bool,
    },
    /// A registered catalog view (`nc_catalog.tables`, …), its rows made
    /// when scanned.
    View {
        /// The view.
        view: ViewRef,
    },
    /// The rows of the previous round of a recursive CTE.
    WorkTable {
        /// Which.
        id: usize,
        /// Name, for `EXPLAIN`.
        name: String,
        /// Columns.
        width: usize,
    },
    /// A partitioned table's rows: its partitions' scans in turn
    /// ([`crate::partition`]).  A partition whose guard is false when the
    /// plan runs (or is made) is not read.
    Append {
        /// The partitions' plans, in bound order.
        inputs: Vec<Plan>,
        /// How the table is partitioned.
        spec: crate::partition::Spec,
        /// Each partition's bound and name.
        parts: Vec<(crate::partition::PartBound, String)>,
        /// Per partition, what must hold (over no row: constants,
        /// parameters, enclosing rows) for it to hold a matching row.
        guards: Vec<Option<Expr>>,
    },
    /// `UNION` / `EXCEPT` / `INTERSECT` `[ALL]`.
    SetOp {
        /// Which.
        op: SetOp,
        /// Left input.
        left: Box<Plan>,
        /// Right input.
        right: Box<Plan>,
        /// Keep duplicates (multiset semantics).
        all: bool,
    },
}

/// Functions that can stand in `FROM`.
#[derive(Clone, Debug, PartialEq)]
pub enum TableFunc {
    /// A registered table function.
    Call(TableFnRef),
    /// A scalar function: its value as one row (the only argument is the
    /// bound call).
    Scalar,
}

/// Join types the executor runs (`RIGHT` is planned as a swapped `LEFT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    /// Inner join.
    Inner,
    /// Left outer join.
    Left,
    /// Full outer join.
    Full,
    /// The left rows some right row matches, once each (an `EXISTS` or
    /// `IN` subquery, unnested): only the left's columns.
    Semi,
    /// The left rows no right row matches (`NOT EXISTS`): only the left's
    /// columns.
    Anti,
}

/// Join algorithms.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinAlgo {
    /// For each left row, every right row.
    NestedLoop,
    /// For each left row, the right input — a table scan by an index whose
    /// key reads the left row (as `Outer` at depth 1) — run for it: an
    /// index nested loop; `on` is checked too.
    Lookup,
    /// A hash table of the right input on `right_keys`, probed with
    /// `left_keys` (equi-join); `on` is checked too.
    Hash {
        /// Keys over the left row.
        left_keys: Vec<Expr>,
        /// Keys over the right row.
        right_keys: Vec<Expr>,
        /// A semi or anti join's table of the left input instead (the
        /// smaller), the right rows marking the left rows they match; the
        /// left rows then given in order, those marked or not.
        build_left: bool,
    },
}

/// How a scan finds rows: an index (primary = `None`), equal values for a
/// prefix of its columns, and a range on the next column.  The scan's
/// filter is always kept, so an access path only has to *include* the
/// matching rows.
#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// Every row.
    Full,
    /// A key range of an index.
    Key {
        /// `None`: the primary index; `Some(i)`: `table.indexes[i]`.
        index: Option<usize>,
        /// Values of the leading index columns.
        prefix: Vec<Expr>,
        /// Lower bound on the next column.
        lower: Bound<Expr>,
        /// Upper bound on the next column.
        upper: Bound<Expr>,
    },
}

/// An aggregate call.
#[derive(Clone, Debug, PartialEq)]
pub struct Agg {
    /// The aggregate.
    pub func: AggRef,
    /// Its arguments (none for `count(*)`).
    pub args: Vec<Expr>,
    /// `DISTINCT`.
    pub distinct: bool,
    /// `ORDER BY` inside the call: the order values are aggregated in.
    pub order: Vec<SortKey>,
}

/// A sort key.
#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    /// The value sorted on.
    pub expr: Expr,
    /// Descending.
    pub desc: bool,
    /// `NULL`s before other values.
    pub nulls_first: bool,
}

impl Plan {
    /// Columns per output row.
    pub fn width(&self) -> usize {
        match self {
            Plan::Scan { table, .. } => table.columns.len(),
            Plan::Values { width, .. } => *width,
            Plan::Project { exprs, .. } => exprs.len(),
            Plan::Join {
                left, right, kind, ..
            } => match kind {
                JoinType::Semi | JoinType::Anti => left.width(),
                _ => left.width() + right.width(),
            },
            Plan::Aggregate { group, aggs, .. } => group.len() + aggs.len(),
            Plan::Filter { input, .. }
            | Plan::Secure { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => input.width(),
            Plan::SetOp { left, .. } => left.width(),
            Plan::Append { inputs, .. } => inputs.first().map_or(0, Plan::width),
            Plan::Cte { plan, .. } => plan.width(),
            Plan::Recursive { base, .. } => base.width(),
            Plan::WorkTable { width, .. } | Plan::Function { width, .. } => *width,
            Plan::View { view } => view.columns().len(),
        }
    }

    /// The plan as indented text, one node per line (`EXPLAIN`).
    pub fn explain(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.explain_into(0, None, &mut out);
        out
    }

    /// The same, `note`'s text after each node's (`EXPLAIN ANALYZE`: the
    /// estimates and what the node did; a scan's own estimate is left
    /// out).
    pub fn explain_annotated(&self, note: &dyn Fn(&Plan) -> String) -> Vec<String> {
        let mut out = Vec::new();
        self.explain_into(0, Some(note), &mut out);
        out
    }

    fn explain_into(
        &self,
        depth: usize,
        note: Option<&dyn Fn(&Plan) -> String>,
        out: &mut Vec<String>,
    ) {
        let pad = "  ".repeat(depth);
        let line = match self {
            Plan::Scan {
                table,
                access,
                filter,
                estimate,
            } => {
                let mut s = match access {
                    Access::Full => format!("Seq Scan on {}", table.name),
                    Access::Key {
                        index,
                        prefix,
                        lower,
                        upper,
                    } => {
                        let (name, cols) = match index {
                            None => ("primary key".to_string(), table.primary_key.clone()),
                            Some(i) => (
                                table.indexes[*i].name.clone(),
                                table.indexes[*i].columns.clone(),
                            ),
                        };
                        let mut conds: Vec<String> = prefix
                            .iter()
                            .zip(&cols)
                            .map(|(e, c)| format!("{} = {}", table.columns[*c].name, show(e)))
                            .collect();
                        if let Some(c) = cols.get(prefix.len()) {
                            let col = &table.columns[*c].name;
                            match lower {
                                Bound::Included(e) => conds.push(format!("{col} >= {}", show(e))),
                                Bound::Excluded(e) => conds.push(format!("{col} > {}", show(e))),
                                Bound::Unbounded => {}
                            }
                            match upper {
                                Bound::Included(e) => conds.push(format!("{col} <= {}", show(e))),
                                Bound::Excluded(e) => conds.push(format!("{col} < {}", show(e))),
                                Bound::Unbounded => {}
                            }
                        }
                        format!(
                            "Index Scan using {name} on {} ({})",
                            table.name,
                            conds.join(" AND ")
                        )
                    }
                };
                if let Some(f) = filter {
                    let _ = write!(s, " filter: {}", show(f));
                }
                if let Some(e) = estimate
                    && note.is_none()
                {
                    let _ = write!(s, " (rows={e:.0})");
                }
                s
            }
            Plan::Values { rows, .. } => format!("Values ({} rows)", rows.len()),
            Plan::Filter { predicate, .. } => format!("Filter: {}", show(predicate)),
            Plan::Secure { policy, .. } => format!("Row Security: {}", show(policy)),
            Plan::Project { exprs, .. } => {
                format!(
                    "Project: {}",
                    exprs.iter().map(show).collect::<Vec<_>>().join(", ")
                )
            }
            Plan::Join { kind, on, algo, .. } => {
                let k = match kind {
                    JoinType::Inner => "",
                    JoinType::Left => "Left ",
                    JoinType::Full => "Full ",
                    JoinType::Semi => "Semi ",
                    JoinType::Anti => "Anti ",
                };
                let a = match algo {
                    JoinAlgo::NestedLoop => "Nested Loop".to_string(),
                    JoinAlgo::Lookup => "Index Nested Loop".to_string(),
                    JoinAlgo::Hash {
                        left_keys,
                        right_keys,
                        build_left,
                    } => format!(
                        "Hash Join{} ({})",
                        if *build_left { " (left hashed)" } else { "" },
                        left_keys
                            .iter()
                            .zip(right_keys)
                            .map(|(l, r)| format!("{} = {}", show(l), show(r)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                match on {
                    Some(on) => format!("{k}{a} on {}", show(on)),
                    None => format!("{k}{a}"),
                }
            }
            Plan::Aggregate { group, aggs, .. } => {
                let mut parts: Vec<String> = aggs
                    .iter()
                    .map(|a| {
                        let arg = if a.args.is_empty() {
                            "*".to_string()
                        } else {
                            a.args.iter().map(show).collect::<Vec<_>>().join(", ")
                        };
                        format!(
                            "{}({}{arg})",
                            a.func.name(),
                            if a.distinct { "DISTINCT " } else { "" }
                        )
                    })
                    .collect();
                if !group.is_empty() {
                    parts.push(format!(
                        "group by {}",
                        group.iter().map(show).collect::<Vec<_>>().join(", ")
                    ));
                }
                format!("Aggregate: {}", parts.join(", "))
            }
            Plan::Sort { keys, .. } => format!(
                "Sort: {}",
                keys.iter()
                    .map(|k| format!("{}{}", show(&k.expr), if k.desc { " DESC" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Plan::Limit { limit, offset, .. } => format!(
                "Limit: {}{}",
                limit.as_ref().map_or("ALL".into(), show),
                offset
                    .as_ref()
                    .map_or(String::new(), |o| format!(" offset {}", show(o)))
            ),
            Plan::Distinct { .. } => "Distinct".into(),
            Plan::Cte { name, .. } => format!("CTE {name}"),
            Plan::Recursive { all, .. } => {
                format!("Recursive Union{}", if *all { " All" } else { "" })
            }
            Plan::WorkTable { name, .. } => format!("WorkTable Scan on {name}"),
            Plan::Function { func, args, .. } => match func {
                TableFunc::Call(f) => format!(
                    "Function Scan on {}({})",
                    f.name(),
                    args.iter().map(show).collect::<Vec<_>>().join(", ")
                ),
                TableFunc::Scalar => format!("Result: {}", show(&args[0])),
            },
            Plan::View { view } => format!("Catalog Scan on {}.{}", view.schema(), view.name()),
            Plan::Append { parts, guards, .. } => {
                let guarded: Vec<String> = parts
                    .iter()
                    .zip(guards)
                    .filter_map(|((_, n), g)| g.as_ref().map(|g| format!("{n} if {}", show(g))))
                    .collect();
                if guarded.is_empty() {
                    "Append".into()
                } else {
                    format!("Append (pruned when run: {})", guarded.join("; "))
                }
            }
            Plan::SetOp { op, all, .. } => {
                let name = match op {
                    SetOp::Union => "Union",
                    SetOp::Except => "Except",
                    SetOp::Intersect => "Intersect",
                };
                if *all {
                    format!("{name} All")
                } else {
                    name.into()
                }
            }
        };
        let line = match note {
            Some(n) => format!("{line}{}", n(self)),
            None => line,
        };
        out.push(format!("{pad}{line}"));
        for child in self.children() {
            child.explain_into(depth + 1, note, out);
        }
        // Subqueries in this node's expressions, after its inputs.
        let mut subs = Vec::new();
        self.for_each_expr(&mut |e| {
            e.walk(&mut |x| {
                if let Expr::Subquery(s) = x {
                    subs.push(&**s);
                }
            })
        });
        for s in subs {
            let kind = match s.kind {
                SubKind::Scalar => "Subquery",
                SubKind::Exists => "Exists",
                SubKind::In(..) => "In",
                SubKind::Array => "Array",
            };
            out.push(format!(
                "{pad}  {kind}{}:",
                if s.correlated { " (correlated)" } else { "" }
            ));
            s.plan.explain_into(depth + 2, note, out);
        }
    }

    /// The inputs of this node.
    pub fn children(&self) -> Vec<&Plan> {
        match self {
            Plan::Scan { .. }
            | Plan::Values { .. }
            | Plan::WorkTable { .. }
            | Plan::Function { .. }
            | Plan::View { .. } => vec![],
            Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => vec![left, right],
            Plan::Append { inputs, .. } => inputs.iter().collect(),
            Plan::Recursive { base, step, .. } => vec![base, step],
            Plan::Cte { plan, .. } => vec![plan],
            Plan::Filter { input, .. }
            | Plan::Secure { input, .. }
            | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => vec![input],
        }
    }

    /// The inputs of this node, each with the levels of rows its `Outer`
    /// references count from here: 1 for an index nested loop's right
    /// side, which reads the left row as the nearest one (its depth 1),
    /// else 0.
    pub fn child_levels(&self) -> Vec<(&Plan, usize)> {
        match self {
            Plan::Join {
                left,
                right,
                algo: JoinAlgo::Lookup,
                ..
            } => vec![(left, 0), (right, 1)],
            p => p.children().into_iter().map(|c| (c, 0)).collect(),
        }
    }

    /// [`child_levels`](Plan::child_levels), to change.
    pub fn child_levels_mut(&mut self) -> Vec<(&mut Plan, usize)> {
        match self {
            Plan::Join {
                left,
                right,
                algo: JoinAlgo::Lookup,
                ..
            } => vec![(left, 0), (right, 1)],
            p => p.children_mut().into_iter().map(|c| (c, 0)).collect(),
        }
    }

    /// The inputs of this node, to change.
    pub fn children_mut(&mut self) -> Vec<&mut Plan> {
        match self {
            Plan::Scan { .. }
            | Plan::Values { .. }
            | Plan::WorkTable { .. }
            | Plan::Function { .. }
            | Plan::View { .. } => vec![],
            Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => vec![left, right],
            Plan::Append { inputs, .. } => inputs.iter_mut().collect(),
            Plan::Recursive { base, step, .. } => vec![base, step],
            Plan::Cte { plan, .. } => vec![plan],
            Plan::Filter { input, .. }
            | Plan::Secure { input, .. }
            | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => vec![input],
        }
    }

    /// [`for_each_expr`](Plan::for_each_expr), each expression to change.
    pub fn for_each_expr_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Plan::Scan { access, filter, .. } => {
                if let Access::Key {
                    prefix,
                    lower,
                    upper,
                    ..
                } = access
                {
                    prefix.iter_mut().for_each(&mut *f);
                    for b in [lower, upper] {
                        if let Bound::Included(e) | Bound::Excluded(e) = b {
                            f(e);
                        }
                    }
                }
                if let Some(e) = filter {
                    f(e);
                }
            }
            Plan::Values { rows, .. } => rows.iter_mut().flatten().for_each(f),
            Plan::Function { args, .. } => args.iter_mut().for_each(f),
            Plan::Filter { predicate, .. } => f(predicate),
            Plan::Secure { policy, .. } => f(policy),
            Plan::Project { exprs, .. } => exprs.iter_mut().for_each(f),
            Plan::Join { on, algo, .. } => {
                if let Some(e) = on {
                    f(e);
                }
                if let JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    ..
                } = algo
                {
                    left_keys.iter_mut().chain(right_keys).for_each(f);
                }
            }
            Plan::Aggregate { group, aggs, .. } => {
                group.iter_mut().for_each(&mut *f);
                for a in aggs {
                    a.args.iter_mut().for_each(&mut *f);
                    a.order.iter_mut().for_each(|k| f(&mut k.expr));
                }
            }
            Plan::Sort { keys, .. } => keys.iter_mut().for_each(|k| f(&mut k.expr)),
            Plan::Limit { limit, offset, .. } => {
                limit.iter_mut().chain(offset).for_each(f);
            }
            Plan::Append { guards, .. } => guards.iter_mut().flatten().for_each(f),
            Plan::Distinct { .. }
            | Plan::SetOp { .. }
            | Plan::Cte { .. }
            | Plan::Recursive { .. }
            | Plan::WorkTable { .. }
            | Plan::View { .. } => {}
        }
    }

    /// Rewrites the references of this plan — `nesting` subqueries below
    /// some level — to that level's row (`Outer` at depth `nesting`): each
    /// becomes `f` of its column, an expression of that level's row lifted
    /// to here.
    pub fn remap_level(&mut self, nesting: usize, f: &dyn Fn(usize) -> Expr) {
        self.for_each_expr_mut(&mut |e| {
            e.walk_mut(&mut |x| match x {
                Expr::Outer { depth, index } if *depth == nesting => {
                    *x = f(*index).lifted(nesting);
                }
                Expr::Subquery(s) => s.plan.remap_level(nesting + 1, f),
                _ => {}
            })
        });
        for (c, below) in self.child_levels_mut() {
            c.remap_level(nesting + below, f);
        }
    }

    /// Adds `by` to the depth of the references of this plan — `nesting`
    /// subqueries below some level — to that level or above.
    pub fn deepen(&mut self, nesting: usize, by: usize) {
        self.for_each_expr_mut(&mut |e| {
            e.walk_mut(&mut |x| match x {
                Expr::Outer { depth, .. } if *depth >= nesting => *depth += by,
                Expr::Subquery(s) => s.plan.deepen(nesting + 1, by),
                _ => {}
            })
        });
        for (c, below) in self.child_levels_mut() {
            c.deepen(nesting + below, by);
        }
    }

    /// Calls `f` on every expression of this node (not its inputs).
    pub fn for_each_expr<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        match self {
            Plan::Scan { access, filter, .. } => {
                if let Access::Key {
                    prefix,
                    lower,
                    upper,
                    ..
                } = access
                {
                    prefix.iter().for_each(&mut *f);
                    for b in [lower, upper] {
                        if let Bound::Included(e) | Bound::Excluded(e) = b {
                            f(e);
                        }
                    }
                }
                if let Some(e) = filter {
                    f(e);
                }
            }
            Plan::Values { rows, .. } => rows.iter().flatten().for_each(f),
            Plan::Function { args, .. } => args.iter().for_each(f),
            Plan::Filter { predicate, .. } => f(predicate),
            Plan::Secure { policy, .. } => f(policy),
            Plan::Project { exprs, .. } => exprs.iter().for_each(f),
            Plan::Join { on, algo, .. } => {
                if let Some(e) = on {
                    f(e);
                }
                if let JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    ..
                } = algo
                {
                    left_keys.iter().chain(right_keys).for_each(f);
                }
            }
            Plan::Aggregate { group, aggs, .. } => {
                group.iter().for_each(&mut *f);
                aggs.iter()
                    .flat_map(|a| a.args.iter().chain(a.order.iter().map(|k| &k.expr)))
                    .for_each(f);
            }
            Plan::Sort { keys, .. } => keys.iter().for_each(|k| f(&k.expr)),
            Plan::Limit { limit, offset, .. } => {
                limit.iter().chain(offset).for_each(f);
            }
            Plan::Append { guards, .. } => guards.iter().flatten().for_each(f),
            Plan::Distinct { .. }
            | Plan::SetOp { .. }
            | Plan::Cte { .. }
            | Plan::Recursive { .. }
            | Plan::WorkTable { .. }
            | Plan::View { .. } => {}
        }
    }

    /// The deepest `Outer` reference in this plan, relative to it: 0 if the
    /// plan only reads its own rows.
    pub fn outer_depth(&self) -> usize {
        let mut max = 0;
        self.outer_depth_at(0, &mut max);
        max
    }

    fn outer_depth_at(&self, nesting: usize, max: &mut usize) {
        self.for_each_expr(&mut |e| {
            e.walk(&mut |x| match x {
                Expr::Outer { depth, .. } if *depth > nesting => *max = (*max).max(depth - nesting),
                Expr::Subquery(s) => s.plan.outer_depth_at(nesting + 1, max),
                _ => {}
            })
        });
        for (c, below) in self.child_levels() {
            c.outer_depth_at(nesting + below, max);
        }
    }
}

/// An expression as text, for `EXPLAIN` (columns as `#n`).
pub fn show(e: &Expr) -> String {
    show_with(e, &|i| format!("#{i}"))
}

/// The same, column `i` as `col(i)` writes it.
pub fn show_with(e: &Expr, col: &dyn Fn(usize) -> String) -> String {
    use crate::types::Value;
    match e {
        Expr::Literal(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Literal(v) => v.render(),
        Expr::Column(i) => col(*i),
        Expr::Outer { depth, index } => format!("outer{depth}.#{index}"),
        Expr::Param(i) => format!("${}", i + 1),
        Expr::Unary(op, e) => format!(
            "{}{}",
            match op {
                crate::ast::UnaryOp::Neg => "-",
                crate::ast::UnaryOp::Plus => "+",
                crate::ast::UnaryOp::Not => "NOT ",
            },
            show_with(e, col)
        ),
        Expr::Binary(op, l, r) => format!(
            "({} {} {})",
            show_with(l, col),
            op.symbol(),
            show_with(r, col)
        ),
        Expr::IsNull(e, n) => format!(
            "{} IS {}NULL",
            show_with(e, col),
            if *n { "NOT " } else { "" }
        ),
        Expr::InList(e, l, n) => format!(
            "{} {}IN ({})",
            show_with(e, col),
            if *n { "NOT " } else { "" },
            l.iter()
                .map(|x| show_with(x, col))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "{} {}{} {}",
            show_with(expr, col),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            show_with(pattern, col)
        ),
        Expr::Case { .. } => "CASE …".into(),
        Expr::Cast(e, t) => format!("{}::{t}", show_with(e, col)),
        Expr::Call(f, args) => format!(
            "{}({})",
            f.name(),
            args.iter()
                .map(|x| show_with(x, col))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Subquery(s) => match &s.kind {
            SubKind::Scalar => "(subquery)".into(),
            SubKind::Exists => "EXISTS (subquery)".into(),
            SubKind::In(e, n) => {
                format!(
                    "{} {}IN (subquery)",
                    show_with(e, col),
                    if *n { "NOT " } else { "" }
                )
            }
            SubKind::Array => "ARRAY(subquery)".into(),
        },
        Expr::Array(items) => format!(
            "ARRAY[{}]",
            items
                .iter()
                .map(|x| show_with(x, col))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Subscript(a, i) => format!("{}[{}]", show_with(a, col), show_with(i, col)),
        Expr::AnyAll {
            left,
            op,
            right,
            all,
        } => format!(
            "{} {} {}({})",
            show_with(left, col),
            op.symbol(),
            if *all { "ALL " } else { "ANY " },
            show_with(right, col)
        ),
    }
}
