//! Hybrid search (PLAN.md §M14.21 T7): rankings fused in one query —
//!
//! ```sql
//! SELECT id FROM docs WHERE tenant = $1
//! ORDER BY rrf(score(body @@ $2), emb <=> $3) DESC LIMIT 10;
//! ```
//!
//! — reciprocal rank fusion (`rrf`, Cormack, Clarke and Büttcher 2009) and
//! a convex combination of min-max normalized scores (`convex`, Bruch, Gai
//! and Ingber 2024), each ranking over its **list**: its first `window`
//! rows of the rows the query keeps, as Elasticsearch's, OpenSearch's and
//! Weaviate's hybrid queries take them (their `rank_window_size`).  A
//! ranking is greatest first, a distance — `<->`, `<=>`, `<#>`, `<+>`, a
//! `…_distance` function, `negative_inner_product` — nearest first; a
//! function whose rows are those of a condition (`score(body @@ q)`:
//! `body @@ q`, [`ScalarFunction::found`]) ranks only those.
//!
//! One plan, by the optimizer ([`fused`]): each list a `Limit` over a
//! `Sort` of the query's scan — which the index on its column ranks
//! (§M14.21 T5a: the text index's first rows by BM25 with their scores,
//! HNSW's nearest) — with a window over it (its rank, or its least and
//! greatest value); the lists' rows `UNION ALL`, one row a candidate by
//! an aggregate over its primary key, and fused.  No ranking is computed
//! again over the candidates.  Without an index the same plan reads
//! every row: the same meaning (HNSW's nearest rows approximate, as
//! everywhere).
//!
//! - `rrf(r1, r2, …)`: Σ 1 / (60 + rank) over the lists the row is in
//!   (`rank()`: ties share one).
//! - `convex(r1, r2, α)`: α·n1 + (1 − α)·n2, each ranking min-max
//!   normalized over its list — (x − min) / (max − min), a distance
//!   (max − x) / (max − min); 1 where all are equal, 0 for a row not in
//!   the list or `NULL` there (OpenSearch's `min_max`, Elasticsearch's
//!   linear retriever, Weaviate's `relativeScoreFusion`).
//!
//! `window` is 100, or the rows the limit and the offset take where they
//! are more.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::ast::{BinaryOp, FrameExclude, FrameUnits, SetOp};
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::plan::{Plan, SortKey, WinBound, WinFrame, WinFunc, WindowFunc};
use crate::types::{DataType, Value};

/// RRF's constant (Cormack et al.'s, everyone's default).
const K: f64 = 60.0;
/// The candidates a ranking gives at least.
pub const WINDOW: i64 = 100;

/// The fusions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Rrf,
    Convex,
}

struct Fusion(Kind);

impl ScalarFunction for Fusion {
    fn name(&self) -> &str {
        match self.0 {
            Kind::Rrf => "rrf",
            Kind::Convex => "convex",
        }
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            Kind::Rrf => 2..=8,
            Kind::Convex => 3..=3,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut sig = vec![Arg::Keep; args.len()];
        if self.0 == Kind::Convex {
            sig[2] = Arg::Convert(DataType::Float);
        }
        Ok(Signature::new(sig, Returns::Type(Some(DataType::Float))))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }

    fn call(&self, _: &CallContext<'_>, _: &[Value]) -> Result<Value> {
        Err(Error::feature(format!(
            "{}() ranks a query's first rows: ORDER BY {}(…) DESC LIMIT k over one table",
            self.name(),
            self.name()
        )))
    }
}

/// Registers `rrf` and `convex`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["rrf"], Arc::new(Fusion(Kind::Rrf)))?;
    r.add_function(&["convex"], Arc::new(Fusion(Kind::Convex)))
}

/// Whether a ranking is a distance, nearest first: the distance operators
/// and functions, `<#>`'s `negative_inner_product` among them.
fn nearest_first(e: &Expr) -> bool {
    let Expr::Call(f, _) = e else { return false };
    let n = f.name();
    ["<->", "<=>", "<#>", "<+>"]
        .iter()
        .any(|op| n.starts_with(op))
        || n.ends_with("_distance")
        || n == "negative_inner_product"
}

/// The scan under row-level security's filter and barrier, if `p` is one.
fn scan_of(p: &mut Plan) -> Option<&mut Plan> {
    match p {
        Plan::Scan { .. } => Some(p),
        Plan::Filter { input, .. } | Plan::Secure { input, .. } => scan_of(input),
        _ => None,
    }
}

/// The scans of `p`: its one, or a partitioned table's — each partition's
/// (§8.16) — where every input of its `Append` is one.
fn scans_of(p: &mut Plan) -> Option<Vec<&mut Plan>> {
    if !matches!(p, Plan::Append { .. }) {
        return scan_of(p).map(|s| vec![s]);
    }
    let Plan::Append { inputs, .. } = p else {
        unreachable!()
    };
    if inputs.is_empty() {
        return None;
    }
    inputs.iter_mut().map(scan_of).collect()
}

/// The literal rows a limit and an offset take, where known.
fn rows_taken(limit: &Expr, offset: Option<&Expr>) -> Option<i64> {
    let n = |e: &Expr| match e {
        Expr::Literal(Value::Int(n)) => Some(*n),
        _ => None,
    };
    Some(n(limit)? + offset.map_or(Some(0), n)?)
}

fn lit(x: f64) -> Expr {
    Expr::Literal(Value::Float(x))
}

fn bin(op: BinaryOp, a: Expr, b: Expr) -> Expr {
    Expr::Binary(op, Box::new(a), Box::new(b))
}

fn case(when: Expr, then: Expr, otherwise: Expr) -> Expr {
    Expr::Case {
        operand: None,
        whens: vec![(when, then)],
        otherwise: Some(Box::new(otherwise)),
    }
}

/// A window over every candidate, ordered by `by`.
fn window(func: WinFunc, args: Vec<Expr>, by: Expr, desc: bool) -> WindowFunc {
    WindowFunc {
        func,
        args,
        partition: Vec::new(),
        order: vec![SortKey {
            expr: by,
            desc,
            nulls_first: false,
        }],
        frame: WinFrame {
            units: FrameUnits::Range,
            start: WinBound::UnboundedPreceding,
            end: WinBound::CurrentRow,
            exclude: FrameExclude::NoOthers,
        },
    }
}

/// `ORDER BY rrf(…) | convex(…) LIMIT k` over one table's scan — a
/// partitioned table's, its partitions' —, as the plan of its candidates
/// ([module docs](self)); any other plan as it is.
pub fn fused(plan: Plan) -> Plan {
    let mut plan = crate::optimizer::map_children(plan, &fused);
    let Plan::Limit {
        input,
        limit: Some(limit),
        offset,
    } = &mut plan
    else {
        return plan;
    };
    let taken = rows_taken(limit, offset.as_ref());
    let Plan::Sort {
        input: sorted,
        keys,
    } = &mut **input
    else {
        return plan;
    };
    let Some(Expr::Column(j)) = keys.first().map(|k| &k.expr) else {
        return plan;
    };
    let j = *j;
    let Plan::Project {
        input: below,
        exprs,
    } = &mut **sorted
    else {
        return plan;
    };
    let Some(call @ Expr::Call(f, args)) = exprs.get(j) else {
        return plan;
    };
    let kind = match f.name() {
        "rrf" => Kind::Rrf,
        "convex" => Kind::Convex,
        _ => return plan,
    };
    let call = call.clone();
    let rankings: Vec<Expr> = match kind {
        Kind::Rrf => args.clone(),
        Kind::Convex => args[..2].to_vec(),
    };
    let alpha = (kind == Kind::Convex).then(|| args[2].clone());
    let mut base = (**below).clone();
    // A partitioned table's partitions have its columns and key.
    let Some(Plan::Scan { table, .. }) = scans_of(&mut base).and_then(|s| s.into_iter().next())
    else {
        return plan;
    };
    let pk = table.primary_key.clone();
    let n = below.width();
    let win = taken.map_or(WINDOW, |t| t.max(WINDOW));
    let cols = |w: usize| (0..w).map(Expr::Column).collect::<Vec<_>>();
    // The candidates' columns: those read above besides the fusion, and
    // the primary key (which tells the others) — all of them where a
    // subquery reads above, or there is no key.
    const FUSED: usize = usize::MAX;
    let marked: Vec<Expr> = exprs
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.walk_mut(&mut |x| {
                if *x == call {
                    *x = Expr::Column(FUSED);
                }
            });
            e
        })
        .collect();
    let keep: Vec<usize> = if pk.is_empty() || marked.iter().any(Expr::has_subquery) {
        (0..n).collect()
    } else {
        let mut k = pk.clone();
        for e in &marked {
            k.extend(e.columns().into_iter().filter(|&c| c != FUSED));
        }
        k.sort_unstable();
        k.dedup();
        k
    };
    let kept = || keep.iter().map(|&c| Expr::Column(c)).collect::<Vec<_>>();
    // Each ranking's slots: its rank (rrf), or its value and its list's
    // least and greatest (convex).
    let m = rankings.len();
    let per = match kind {
        Kind::Rrf => 1,
        Kind::Convex => 3,
    };
    let reg = crate::ext::Registry::shared_builtin();
    let agg = |name: &str| reg.aggregate(name).expect("a built-in aggregate").clone();
    let whole = |func: WinFunc, x: Expr| WindowFunc {
        func,
        args: vec![x],
        partition: Vec::new(),
        order: Vec::new(),
        frame: WinFrame {
            units: FrameUnits::Range,
            start: WinBound::UnboundedPreceding,
            end: WinBound::UnboundedFollowing,
            exclude: FrameExclude::NoOthers,
        },
    };
    // Each ranking's first rows — the scan sorted by it, its ties broken
    // by the primary key, a function of a condition's rows those of the
    // condition — with its slots, the others' `NULL`; all of them.
    let mut lists = Vec::new();
    for (i, r) in rankings.iter().enumerate() {
        let mut scan = base.clone();
        if let Expr::Call(g, a) = r
            && let Some(cond) = g.found(a)
        {
            for s in scans_of(&mut scan).into_iter().flatten() {
                if let Plan::Scan { filter, .. } = s {
                    let mut conds = Vec::new();
                    if let Some(f) = filter.take() {
                        crate::binder::conjuncts(f, &mut conds);
                    }
                    conds.push(cond.clone());
                    *filter = crate::binder::conjunction(conds);
                }
            }
        }
        let mut with = cols(n);
        with.push(r.clone());
        let near = nearest_first(r);
        let mut order = vec![SortKey {
            expr: Expr::Column(n),
            desc: !near,
            nulls_first: false,
        }];
        order.extend(pk.iter().map(|&c| SortKey {
            expr: Expr::Column(c),
            desc: false,
            nulls_first: false,
        }));
        let first = Plan::Limit {
            input: Box::new(Plan::Sort {
                input: Box::new(Plan::Project {
                    input: Box::new(scan),
                    exprs: with,
                }),
                keys: order,
            }),
            limit: Some(Expr::Literal(Value::Int(win))),
            offset: None,
        };
        // Sorted by the ranking first, as the window's order wants.
        let x = Expr::Column(n);
        let funcs = match kind {
            Kind::Rrf => vec![window(WinFunc::Rank, vec![], x.clone(), !near)],
            Kind::Convex => vec![
                whole(WinFunc::Agg(agg("min")), x.clone()),
                whole(WinFunc::Agg(agg("max")), x.clone()),
            ],
        };
        let mut slots = vec![Expr::Literal(Value::Null); per * m];
        match kind {
            Kind::Rrf => slots[i] = Expr::Column(n + 1),
            Kind::Convex => {
                slots[per * i] = x;
                slots[per * i + 1] = Expr::Column(n + 1);
                slots[per * i + 2] = Expr::Column(n + 2);
            }
        }
        let mut exprs = kept();
        exprs.extend(slots);
        lists.push(Plan::Project {
            input: Box::new(Plan::Window {
                input: Box::new(first),
                funcs,
            }),
            exprs,
        });
    }
    // One row a candidate: its slots of each list it is in.
    let union = lists
        .into_iter()
        .reduce(|left, right| Plan::SetOp {
            op: SetOp::Union,
            left: Box::new(left),
            right: Box::new(right),
            all: true,
        })
        .expect("two rankings");
    let at = keep.len();
    let stack = Plan::Aggregate {
        input: Box::new(union),
        group: cols(at),
        aggs: (0..per * m)
            .map(|s| crate::plan::Agg {
                func: agg("max"),
                args: vec![Expr::Column(at + s)],
                distinct: false,
                order: Vec::new(),
            })
            .collect(),
    };
    let float = |e: Expr| Expr::Cast(Box::new(e), DataType::Float);
    let value = match kind {
        Kind::Rrf => (0..m)
            .map(|i| {
                let rank = Expr::Column(at + i);
                case(
                    Expr::IsNull(Box::new(rank.clone()), false),
                    lit(0.0),
                    bin(
                        BinaryOp::Div,
                        lit(1.0),
                        bin(BinaryOp::Add, lit(K), float(rank)),
                    ),
                )
            })
            .reduce(|a, b| bin(BinaryOp::Add, a, b))
            .expect("two rankings"),
        Kind::Convex => {
            let norm = |i: usize| {
                let x = float(Expr::Column(at + per * i));
                let (lo, hi) = (
                    float(Expr::Column(at + per * i + 1)),
                    float(Expr::Column(at + per * i + 2)),
                );
                let span = bin(BinaryOp::Sub, hi.clone(), lo.clone());
                let up = if nearest_first(&rankings[i]) {
                    bin(BinaryOp::Sub, hi.clone(), x.clone())
                } else {
                    bin(BinaryOp::Sub, x.clone(), lo.clone())
                };
                case(
                    Expr::IsNull(Box::new(x), false),
                    lit(0.0),
                    case(
                        bin(BinaryOp::Eq, hi, lo),
                        lit(1.0),
                        bin(BinaryOp::Div, up, span),
                    ),
                )
            };
            let a = alpha.expect("convex's weight");
            bin(
                BinaryOp::Add,
                bin(BinaryOp::Mul, a.clone(), norm(0)),
                bin(BinaryOp::Mul, bin(BinaryOp::Sub, lit(1.0), a), norm(1)),
            )
        }
    };
    let at_kept = |c: usize| match c {
        FUSED => value.clone(),
        c => Expr::Column(keep.binary_search(&c).expect("a kept column")),
    };
    *exprs = marked.iter().map(|e| e.map_columns(&at_kept)).collect();
    **below = stack;
    plan
}
