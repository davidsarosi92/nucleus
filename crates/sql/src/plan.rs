//! Query plans: the tree the binder builds, the optimizer rewrites and the
//! executor runs.  Each node's output is a row of `width()` values.

use std::fmt::Write;
use std::ops::Bound;

use crate::ast::SetOp;
use crate::catalog::Table;
use crate::expr::{Expr, SubKind};

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
    /// The rows of the previous round of a recursive CTE.
    WorkTable {
        /// Which.
        id: usize,
        /// Name, for `EXPLAIN`.
        name: String,
        /// Columns.
        width: usize,
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

/// Join types the executor runs (`RIGHT` is planned as a swapped `LEFT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    /// Inner join.
    Inner,
    /// Left outer join.
    Left,
    /// Full outer join.
    Full,
}

/// Join algorithms.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinAlgo {
    /// For each left row, every right row.
    NestedLoop,
    /// A hash table of the right input on `right_keys`, probed with
    /// `left_keys` (equi-join); `on` is checked too.
    Hash {
        /// Keys over the left row.
        left_keys: Vec<Expr>,
        /// Keys over the right row.
        right_keys: Vec<Expr>,
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

/// An aggregate.
#[derive(Clone, Debug, PartialEq)]
pub struct Agg {
    /// The function.
    pub func: AggFunc,
    /// Its argument (`None` for `count(*)`).
    pub arg: Option<Expr>,
    /// `DISTINCT`.
    pub distinct: bool,
}

/// Aggregate functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// `count`.
    Count,
    /// `sum`.
    Sum,
    /// `avg`.
    Avg,
    /// `min`.
    Min,
    /// `max`.
    Max,
}

impl AggFunc {
    /// The function called `name`.
    pub fn lookup(name: &str) -> Option<Self> {
        Some(match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
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
            Plan::Join { left, right, .. } => left.width() + right.width(),
            Plan::Aggregate { group, aggs, .. } => group.len() + aggs.len(),
            Plan::Filter { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => input.width(),
            Plan::SetOp { left, .. } => left.width(),
            Plan::Cte { plan, .. } => plan.width(),
            Plan::Recursive { base, .. } => base.width(),
            Plan::WorkTable { width, .. } => *width,
        }
    }

    /// The plan as indented text, one node per line (`EXPLAIN`).
    pub fn explain(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.explain_into(0, &mut out);
        out
    }

    fn explain_into(&self, depth: usize, out: &mut Vec<String>) {
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
                if let Some(e) = estimate {
                    let _ = write!(s, " (rows={e:.0})");
                }
                s
            }
            Plan::Values { rows, .. } => format!("Values ({} rows)", rows.len()),
            Plan::Filter { predicate, .. } => format!("Filter: {}", show(predicate)),
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
                };
                let a = match algo {
                    JoinAlgo::NestedLoop => "Nested Loop".to_string(),
                    JoinAlgo::Hash {
                        left_keys,
                        right_keys,
                    } => format!(
                        "Hash Join ({})",
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
                        let arg = a.arg.as_ref().map_or("*".to_string(), show);
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
        out.push(format!("{pad}{line}"));
        for child in self.children() {
            child.explain_into(depth + 1, out);
        }
        // Subqueries in this node's expressions, after its inputs.
        let mut subs = Vec::new();
        self.for_each_expr(&mut |e| {
            e.walk(&mut |x| {
                if let Expr::Subquery(s) = x {
                    subs.push(s.clone());
                }
            })
        });
        for s in subs {
            let kind = match s.kind {
                SubKind::Scalar => "Subquery",
                SubKind::Exists => "Exists",
                SubKind::In(..) => "In",
            };
            out.push(format!(
                "{pad}  {kind}{}:",
                if s.correlated { " (correlated)" } else { "" }
            ));
            s.plan.explain_into(depth + 2, out);
        }
    }

    /// The inputs of this node.
    pub fn children(&self) -> Vec<&Plan> {
        match self {
            Plan::Scan { .. } | Plan::Values { .. } | Plan::WorkTable { .. } => vec![],
            Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => vec![left, right],
            Plan::Recursive { base, step, .. } => vec![base, step],
            Plan::Cte { plan, .. } => vec![plan],
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => vec![input],
        }
    }

    /// Calls `f` on every expression of this node (not its inputs).
    pub fn for_each_expr(&self, f: &mut dyn FnMut(&Expr)) {
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
            Plan::Filter { predicate, .. } => f(predicate),
            Plan::Project { exprs, .. } => exprs.iter().for_each(f),
            Plan::Join { on, algo, .. } => {
                if let Some(e) = on {
                    f(e);
                }
                if let JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                } = algo
                {
                    left_keys.iter().chain(right_keys).for_each(f);
                }
            }
            Plan::Aggregate { group, aggs, .. } => {
                group.iter().for_each(&mut *f);
                aggs.iter().filter_map(|a| a.arg.as_ref()).for_each(f);
            }
            Plan::Sort { keys, .. } => keys.iter().for_each(|k| f(&k.expr)),
            Plan::Limit { limit, offset, .. } => {
                limit.iter().chain(offset).for_each(f);
            }
            Plan::Distinct { .. }
            | Plan::SetOp { .. }
            | Plan::Cte { .. }
            | Plan::Recursive { .. }
            | Plan::WorkTable { .. } => {}
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
        for c in self.children() {
            c.outer_depth_at(nesting, max);
        }
    }
}

/// An expression as text, for `EXPLAIN`.
pub fn show(e: &Expr) -> String {
    use crate::types::Value;
    match e {
        Expr::Literal(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Literal(v) => v.render(),
        Expr::Column(i) => format!("#{i}"),
        Expr::Outer { depth, index } => format!("outer{depth}.#{index}"),
        Expr::Param(i) => format!("${}", i + 1),
        Expr::Unary(op, e) => format!(
            "{}{}",
            match op {
                crate::ast::UnaryOp::Neg => "-",
                crate::ast::UnaryOp::Plus => "+",
                crate::ast::UnaryOp::Not => "NOT ",
            },
            show(e)
        ),
        Expr::Binary(op, l, r) => format!("({} {} {})", show(l), op.symbol(), show(r)),
        Expr::IsNull(e, n) => format!("{} IS {}NULL", show(e), if *n { "NOT " } else { "" }),
        Expr::InList(e, l, n) => format!(
            "{} {}IN ({})",
            show(e),
            if *n { "NOT " } else { "" },
            l.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "{} {}{} {}",
            show(expr),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            show(pattern)
        ),
        Expr::Case { .. } => "CASE …".into(),
        Expr::Cast(e, t) => format!("{}::{t}", show(e)),
        Expr::Func(f, args) => format!(
            "{}({})",
            f.name(),
            args.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
        Expr::Subquery(s) => match &s.kind {
            SubKind::Scalar => "(subquery)".into(),
            SubKind::Exists => "EXISTS (subquery)".into(),
            SubKind::In(e, n) => {
                format!("{} {}IN (subquery)", show(e), if *n { "NOT " } else { "" })
            }
        },
    }
}
