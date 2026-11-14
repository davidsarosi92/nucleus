//! The optimizer: rewrites a bound plan into a cheaper one with the same
//! result.
//!
//! - **Predicate pushdown**: filter conjuncts move as close to the scans as
//!   their columns allow — through projections, into join sides (respecting
//!   outer-join semantics) and into join conditions.
//! - **Subqueries unnested** (`unnest`): `EXISTS`, `NOT EXISTS` and `IN`
//!   conjuncts become semi and anti joins where they can.
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
//!   (up to 8) over the subsets the conditions connect (a cross product
//!   only where they leave the chain in pieces), with the smaller input as
//!   the hash build side.  Without statistics, the rule-based choice and the written
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
    let plan = push(crate::unnest::unnest(plan), &stats);
    let plan = sink(reorder(plan, &stats), &stats);
    let plan = lookups(crate::eager::eager(plan, &stats), &stats);
    let plan = crate::narrow::narrow(sorted(crate::distinct::two_level(plan)));
    let plan = crate::hybrid::fused(plan);
    postponed(ranked(
        method_filters(adaptive(same_rows(plan), &stats)),
        &stats,
    ))
}

/// The values only the output needs computed after the limit, on the
/// rows it keeps: under `LIMIT`, a projection's expressions the sort does
/// not order by — `ts_headline(body, q)` beside `ORDER BY ts_rank(…)
/// LIMIT 10` — move above it, the sort carrying the columns they read
/// (PostgreSQL's `make_sort_input_target`; PLAN.md §M14.21 T8b).  A
/// projection right above takes them in.
fn postponed(plan: Plan) -> Plan {
    let plan = map_children(plan, &postponed);
    match plan {
        Plan::Limit {
            input,
            limit: Some(limit),
            offset,
        } => match postpone(*input) {
            Ok((sort, above)) => Plan::Project {
                input: Box::new(Plan::Limit {
                    input: Box::new(sort),
                    limit: Some(limit),
                    offset,
                }),
                exprs: above,
            },
            Err(sort) => Plan::Limit {
                input: sort,
                limit: Some(limit),
                offset,
            },
        },
        // The projection the limit's output had, over the one it has now.
        Plan::Project { input, exprs } => match *input {
            Plan::Project {
                input: limit,
                exprs: above,
            } if matches!(*limit, Plan::Limit { .. }) => {
                let mut uses = vec![0usize; above.len()];
                for e in &exprs {
                    for c in e.columns() {
                        uses[c] += 1;
                    }
                }
                if uses
                    .iter()
                    .zip(&above)
                    .any(|(&n, e)| n > 1 && !matches!(e, Expr::Column(_)))
                {
                    return Plan::Project {
                        input: Box::new(Plan::Project {
                            input: limit,
                            exprs: above,
                        }),
                        exprs,
                    };
                }
                Plan::Project {
                    input: limit,
                    exprs: exprs
                        .iter()
                        .map(|e| e.map_columns(&|c| above[c].clone()))
                        .collect(),
                }
            }
            other => Plan::Project {
                input: Box::new(other),
                exprs,
            },
        },
        other => other,
    }
}

/// A sort of a projection with the expressions it does not order by
/// taken out: the sort, and the expressions over its rows that give the
/// projection's columns (`Err`: the sort as it was).
fn postpone(sort: Plan) -> std::result::Result<(Plan, Vec<Expr>), Box<Plan>> {
    let Plan::Sort { input, keys } = sort else {
        return Err(Box::new(sort));
    };
    let Plan::Project {
        input: below,
        exprs,
    } = *input
    else {
        return Err(Box::new(Plan::Sort { input, keys }));
    };
    let mut ordered = vec![false; exprs.len()];
    for k in &keys {
        for c in k.expr.columns() {
            ordered[c] = true;
        }
    }
    let later: Vec<bool> = exprs
        .iter()
        .zip(&ordered)
        .map(|(e, &o)| !o && !matches!(e, Expr::Column(_) | Expr::Literal(_)) && !e.has_subquery())
        .collect();
    if !later.contains(&true) {
        return Err(Box::new(Plan::Sort {
            input: Box::new(Plan::Project {
                input: below,
                exprs,
            }),
            keys,
        }));
    }
    let mut inner: Vec<Expr> = Vec::new();
    let mut at = vec![0usize; exprs.len()];
    for (i, e) in exprs.iter().enumerate() {
        if !later[i] {
            at[i] = inner.len();
            inner.push(e.clone());
        }
    }
    let mut read = std::collections::HashMap::new();
    for (i, e) in exprs.iter().enumerate() {
        if later[i] {
            for c in e.columns() {
                let col = Expr::Column(c);
                let n = inner.iter().position(|x| *x == col).unwrap_or_else(|| {
                    inner.push(col);
                    inner.len() - 1
                });
                read.insert(c, n);
            }
        }
    }
    let above = exprs
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if later[i] {
                e.map_columns(&|c| Expr::Column(read[&c]))
            } else {
                Expr::Column(at[i])
            }
        })
        .collect();
    let keys = keys
        .into_iter()
        .map(|k| crate::plan::SortKey {
            expr: k.expr.map_columns(&|c| Expr::Column(at[c])),
            ..k
        })
        .collect();
    Ok((
        Plan::Sort {
            input: Box::new(Plan::Project {
                input: below,
                exprs: inner,
            }),
            keys,
        },
        above,
    ))
}

/// A scan whose filter has a condition an index method finds the rows of
/// exactly — `col @@ q` with a text index on `col`, `q` fixed for the
/// scan, bound with the index's operand ([`crate::binder`]) — reads them
/// through the index ([`Access::Method`]) instead of every row (or a key
/// range of more than [`METHOD_OVER_RANGE`] rows): the condition costs an
/// analysis of each row's text, the index a read of its postings and of
/// the rows found.  Under a row-level security barrier the statement's
/// condition may choose the rows so too, as it may narrow a key range
/// (evaluated once, on no row), and stays above the barrier: where the
/// index cannot tell the rows it is not checked below it.
fn method_filters(plan: Plan) -> Plan {
    let mut plan = map_children(plan, &method_filters);
    match &mut plan {
        Plan::Scan {
            table,
            access,
            filter,
            estimate,
        } => {
            let Some(whole) = filter.as_ref() else {
                return plan;
            };
            let mut conds = Vec::new();
            conjuncts(whole.clone(), &mut conds);
            if let Some((n, served)) = served(table, access, *estimate, &conds, false) {
                conds.remove(n);
                *access = served;
                *filter = conjunction(conds);
            }
        }
        Plan::Filter { input, predicate } => {
            let Plan::Secure { input: scan, .. } = &mut **input else {
                return plan;
            };
            let Plan::Scan {
                table,
                access,
                estimate,
                ..
            } = &mut **scan
            else {
                return plan;
            };
            let mut conds = Vec::new();
            conjuncts(predicate.clone(), &mut conds);
            if let Some((_, served)) = served(table, access, *estimate, &conds, true) {
                *access = served;
            }
        }
        _ => {}
    }
    plan
}

/// The first of `conds` an index method of `table` serves where the scan
/// reads every row (or a key range past [`METHOD_OVER_RANGE`]): its
/// position, and the access path through the index (`above`: the
/// condition stays above the scan, too).
fn served(
    table: &Table,
    access: &Access,
    estimate: Option<f64>,
    conds: &[Expr],
    above: bool,
) -> Option<(usize, Access)> {
    match access {
        Access::Full => {}
        // Without statistics, an equality on every column of an index's
        // key stays (as `choose_access` prefers it then): a method's
        // condition with a parameter has no estimate either, and ranking
        // or checking every row it matches across the table — a memory
        // store's every subject — costs what the key's few rows do not.
        Access::Key { index, prefix, .. }
            if estimate.is_none()
                && !prefix.is_empty()
                && prefix.len()
                    == index
                        .map_or(table.primary_key.len(), |i| table.indexes[i].columns.len()) =>
        {
            return None;
        }
        Access::Key { .. } if estimate.is_none_or(|e| e > METHOD_OVER_RANGE) => {}
        _ => return None,
    }
    for (n, c) in conds.iter().enumerate() {
        let Expr::Call(f, args) = c else { continue };
        let Some((Expr::Column(col), rest)) = args.split_first() else {
            continue;
        };
        if !rest.iter().all(fixed) {
            continue;
        }
        let found = table.indexes.iter().position(|ix| {
            ix.columns.first() == Some(col)
                && crate::index_method::of(ix).ok().flatten().is_some_and(|m| {
                    m.serves(ix, f.name())
                        && match m.operand(ix, f.name()) {
                            Some(v) => matches!(rest.last(), Some(Expr::Literal(x)) if *x == v),
                            None => true,
                        }
                })
        });
        let Some(index) = found else { continue };
        let served = crate::plan::Served {
            function: f.name().to_string(),
            args: rest.to_vec(),
            cond: c.clone(),
            above,
        };
        return Some((
            n,
            Access::Method {
                index,
                served: Some(served),
                rank: None,
            },
        ));
    }
    None
}

/// The rows of a key range past which a condition an index method serves
/// is found by the method instead.
const METHOD_OVER_RANGE: f64 = 1000.0;

/// `ORDER BY f(col, …) LIMIT k` (and an offset's rows) over a scan of a
/// table with an index method that ranks its rows by `f`
/// ([`crate::index_method::IndexMethod::ranks`] — HNSW by its operator
/// class's distance, nearest first; a text index by BM25's `score(col @@
/// q)`, greatest first): the index gives the first `k` rows that pass the
/// scan's conditions ([`Access::Method`] with a [`crate::plan::MethodRank`]) where
/// that costs less than the scan it has, by the method's cost
/// ([`crate::index_method::IndexMethod::rank_cost`], in rows read) against
/// the rows the scan reads.  A scan through a B-tree's range of the
/// filter, or through another method's condition, always does: its rows
/// are ranked among (PLAN.md §M14.12 V5, §M14.21 T5a), or checked where
/// they turn out many.  The sort and the limit stay: the scan gives at
/// least `k` rows, or every row where the index cannot find them.  A
/// table not analyzed has the rows its index holds.  `f`'s other values
/// must be the same for every row ([`fixed`]); a function of two (a
/// distance) may have the column on either side.  One pass for every
/// method (§M14.21 T5a).
fn ranked(plan: Plan, st: &Stats) -> Plan {
    let mut plan = map_children(plan, &|c| ranked(c, st));
    let Plan::Limit {
        input,
        limit: Some(limit),
        offset,
    } = &mut plan
    else {
        return plan;
    };
    let Plan::Sort {
        input: sorted,
        keys,
    } = &mut **input
    else {
        return plan;
    };
    let Some(key) = keys.first() else {
        return plan;
    };
    let (exprs, below) = match &mut **sorted {
        Plan::Project { input, exprs } => (Some(&*exprs), &mut **input),
        other => (None, other),
    };
    let ranked = match (&key.expr, exprs) {
        (Expr::Column(j), Some(exprs)) => exprs.get(*j),
        (e, None) => Some(e),
        _ => None,
    };
    let Some(call @ Expr::Call(f, args)) = ranked else {
        return plan;
    };
    let call = call.clone();
    let (column, rest) = match args.as_slice() {
        [Expr::Column(c), rest @ ..] if rest.iter().all(fixed) => (*c, rest.to_vec()),
        [q, Expr::Column(c)] if fixed(q) => (*c, vec![q.clone()]),
        _ => return plan,
    };
    let count = match offset {
        None => limit.clone(),
        Some(o) => Expr::Binary(BinaryOp::Add, Box::new(limit.clone()), Box::new(o.clone())),
    };
    let k = match (&*limit, offset.as_ref()) {
        (Expr::Literal(crate::Value::Int(n)), None) => *n as f64,
        (Expr::Literal(crate::Value::Int(n)), Some(Expr::Literal(crate::Value::Int(o)))) => {
            (*n + *o) as f64
        }
        _ => 10.0,
    };
    // The ranked function's value read from the index where it gives it
    // (§M14.21 T5c): computed by a projection — of a partitioned table's
    // scans where each partition's gives it.
    let values = exprs.is_some();
    let want = Want {
        column,
        function: f.name().to_string(),
        args: rest,
        desc: key.desc,
        nulls_first: key.nulls_first,
        count,
        k,
        call: values.then(|| call.clone()),
    };
    // A partitioned table: each partition's own index searched for `k`
    // rows, the sort and the limit above taking the first of them.
    let valued = match below {
        Plan::Append { inputs, .. } => {
            let before = want.call.is_some().then(|| inputs.clone());
            let at: Vec<Option<usize>> =
                inputs.iter_mut().map(|p| rank_scan(p, &want, st)).collect();
            match (at.first().copied().flatten(), before) {
                (Some(a), _) if at.iter().all(|x| *x == Some(a)) => Some(a),
                // Not every partition gives the value (its rows would be
                // of another width): none does.
                (_, Some(before)) => {
                    *inputs = before;
                    let want = Want { call: None, ..want };
                    for p in inputs {
                        rank_scan(p, &want, st);
                    }
                    None
                }
                _ => None,
            }
        }
        other => rank_scan(other, &want, st),
    };
    // The projection reads the value where the scan's rows carry it.
    if let Some(at) = valued
        && let Plan::Limit { input, .. } = &mut plan
        && let Plan::Sort { input, .. } = &mut **input
        && let Plan::Project { exprs, .. } = &mut **input
    {
        for e in exprs {
            e.walk_mut(&mut |x| {
                if *x == call {
                    *x = Expr::Column(at);
                }
            });
        }
    }
    plan
}

/// Whether `e` is the same for every row of a scan: no column, nothing
/// volatile, no subquery that reads the scan's row (an uncorrelated one
/// runs once a statement — `(SELECT v FROM items WHERE id = $1)`, the
/// query vector of "more like this" —, a correlated one reading only
/// enclosing rows once a scan).
fn fixed(e: &Expr) -> bool {
    let mut ok = true;
    e.walk(&mut |x| match x {
        Expr::Column(_) => ok = false,
        Expr::Call(f, _) if f.volatility() == crate::ext::Volatility::Volatile => ok = false,
        Expr::Subquery(s) if !crate::expr::row_free(s) => ok = false,
        _ => {}
    });
    ok
}

/// What a ranking asks: `count` rows (`k`, as far as known) first by
/// `function` of `column` and `args`.
struct Want {
    column: usize,
    function: String,
    args: Vec<Expr>,
    desc: bool,
    nulls_first: bool,
    count: Expr,
    k: f64,
    /// The call ranked by, where the plan would read its value from the
    /// scan's rows.
    call: Option<Expr>,
}

/// A scan (under a row-level security barrier, perhaps) ranked by an
/// index for `want`, where one ranks by its order and it costs less
/// ([`ranked`]): the column its rows carry the ranked value in, where
/// they do.
fn rank_scan(node: &mut Plan, want: &Want, st: &Stats) -> Option<usize> {
    let mut scan = node;
    // Under row-level security: the policy, then the statement's filter
    // above its barrier — conditions the ranking checks its rows against
    // in that order, so it keeps going until `k` rows the session may see
    // pass, and evaluates no predicate of the statement on a row the
    // policy hides.
    let mut checks = Vec::new();
    let above =
        matches!(scan, Plan::Filter { input, .. } if matches!(**input, Plan::Secure { .. }));
    if above {
        let Plan::Filter { input, predicate } = scan else {
            return None;
        };
        checks.push(predicate.clone());
        scan = &mut **input;
    }
    if matches!(scan, Plan::Secure { .. }) {
        let Plan::Secure { input, policy } = scan else {
            return None;
        };
        checks.insert(0, policy.clone());
        scan = &mut **input;
    }
    if checks.iter().any(Expr::has_subquery) {
        return None;
    }
    let Plan::Scan {
        table,
        access,
        filter,
        estimate,
    } = scan
    else {
        return None;
    };
    // The index whose condition the scan reads: it ranks its own rows.
    let own = match access {
        Access::Method {
            index,
            served: Some(s),
            rank: None,
        } => Some((*index, s.function.clone())),
        Access::Method { .. } => return None,
        _ => None,
    };
    // A statement's condition the index serves from above a barrier: its
    // rows all pass it, so it is not checked on each again.
    if let Access::Method {
        served: Some(s), ..
    } = &*access
        && s.above
        && let Some(last) = checks.last_mut()
    {
        let mut conds = Vec::new();
        conjuncts(last.clone(), &mut conds);
        conds.retain(|c| *c != s.cond);
        match conjunction(conds) {
            Some(c) => *last = c,
            None => {
                checks.pop();
            }
        }
    }
    // The conditions the rows must pass, as conjuncts: an index of more
    // columns (a partitioned HNSW's) ranks where they hold each of its
    // other columns equal to a value fixed for the scan (a policy's, too —
    // a tenant's).
    let mut conds = Vec::new();
    for f in filter.iter().chain(&checks) {
        conjuncts(f.clone(), &mut conds);
    }
    let equal = |c: usize| {
        conds.iter().find_map(|p| match p {
            Expr::Binary(BinaryOp::Eq, a, b) => match (&**a, &**b) {
                (Expr::Column(x), v) | (v, Expr::Column(x)) if *x == c && fixed(v) => {
                    Some((v.clone(), p.clone()))
                }
                _ => None,
            },
            _ => None,
        })
    };
    // The scan's own index where it ranks, else the one with the most
    // columns fixed: the smallest graph.
    let (index, method, partition) = table
        .indexes
        .iter()
        .enumerate()
        .filter_map(|(i, ix)| {
            if ix.columns.first() != Some(&want.column) {
                return None;
            }
            let m = crate::index_method::of(ix).ok().flatten()?;
            if !m.ranks(ix, &want.function, want.desc, want.nulls_first) {
                return None;
            }
            let partition: Option<Vec<(Expr, Expr)>> =
                ix.columns[1..].iter().map(|&c| equal(c)).collect();
            Some((i, m, partition?))
        })
        .max_by_key(|(i, _, p)| {
            (
                own.as_ref().is_some_and(|o| o.0 == *i),
                p.len(),
                std::cmp::Reverse(*i),
            )
        })?;
    let own = own.filter(|o| o.0 == index);
    // The share of the table's rows the index ranks.
    let part_sel = partition
        .iter()
        .map(|(_, p)| st.selectivity(table, p))
        .product::<f64>();
    // The costs, in rows read; a table not analyzed has the rows its
    // index holds (a partitioned one's graph, many: the search finds out),
    // and keeps a key range its filter chose (as `choose_access` prefers
    // one without statistics).
    let rows = match st.of(table) {
        Some(_) => st.rows(table) * part_sel,
        None if matches!(access, Access::Key { .. }) => return None,
        None if !partition.is_empty() => 1e12,
        None => match method.rows(st.txn(), table, index) {
            Ok(Some(n)) => n as f64,
            _ => return None,
        },
    };
    let sel = (filter
        .iter()
        .chain(&checks)
        .map(|f| st.selectivity(table, f))
        .product::<f64>()
        / part_sel.max(1e-12))
    .min(1.0)
    .max(1.0 / rows.max(1.0));
    let k = want.k;
    // A B-tree's range of the filter, another method's condition: its
    // rows ranked among (their entries read, `k` rows), or checked where
    // they turn out many — never more than its scan reads.
    let within = match access {
        Access::Key { index: Some(_), .. } => Some(Box::new(access.clone())),
        Access::Method { .. } if own.is_none() => Some(Box::new(access.clone())),
        _ => None,
    };
    if within.is_none() {
        let scanned = match access {
            Access::Full => rows,
            Access::Key { .. } | Access::Method { .. } => estimate.unwrap_or(rows),
        };
        let cost = method.rank_cost(
            &table.indexes[index],
            &crate::index_method::RankCost {
                function: &want.function,
                served: own.as_ref().map(|o| o.1.as_str()),
                rows: if own.is_some() { scanned } else { rows },
                k,
                share: sel,
            },
        );
        if cost.is_none_or(|c| c > scanned) {
            return None;
        }
    }
    let rank = crate::plan::MethodRank {
        function: want.function.clone(),
        args: want.args.clone(),
        desc: want.desc,
        count: want.count.clone(),
        within,
        share: sel.min(1.0),
        checks,
        partition: partition.into_iter().map(|(v, _)| v).collect(),
        value: want
            .call
            .clone()
            .filter(|_| method.values(&table.indexes[index], &want.function)),
    };
    let at = rank.value.as_ref().map(|_| table.columns.len());
    match access {
        Access::Method { rank: r, .. } if own.is_some() => *r = Some(rank),
        _ => {
            *access = Access::Method {
                index,
                served: None,
                rank: Some(rank),
            }
        }
    }
    *estimate = Some(estimate.unwrap_or(rows).min(k));
    at
}

/// Inner hash joins whose left side is a table an index of the keys could
/// look up, given the lookups to choose instead as they run when the right
/// side turns out to have fewer rows than where the two cost the same
/// ([`crate::plan::Adaptive`]): a right side estimated larger than it is
/// (a `HAVING`'s third, a filter's guess) then costs what the right
/// estimate would have.  After narrowing, so the lookups read the columns
/// the join's left side has.  Not where the left side reads an enclosing
/// row (its lookups would read it a level further) or has a subquery (the
/// plan's walks do not see the lookups; `prune` does).
fn adaptive(plan: Plan, st: &Stats) -> Plan {
    match map_children(plan, &|c| adaptive(c, st)) {
        Plan::Join {
            left,
            right,
            kind: JoinType::Inner,
            on,
            algo:
                JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    build_left: false,
                    adaptive: None,
                },
        } => {
            let mut subquery = false;
            left.for_each_expr(&mut |e| subquery |= e.has_subquery());
            let other = (left.outer_depth() == 0 && !subquery)
                .then(|| lookup_plan(&left, &right_keys, &left_keys, false, st))
                .flatten()
                .and_then(|(lookup, costs)| {
                    let below = costs.below();
                    let (lw, rw) = (left.width(), right.width());
                    let flip = |c: usize| Expr::Column(if c < lw { c + rw } else { c - lw });
                    (below > 0).then(|| {
                        Box::new(crate::plan::Adaptive {
                            lookup,
                            on: on.as_ref().map(|e| e.map_columns(&flip)),
                            below,
                        })
                    })
                });
            Plan::Join {
                left,
                right,
                kind: JoinType::Inner,
                on,
                algo: JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    build_left: false,
                    adaptive: other,
                },
            }
        }
        p => p,
    }
}

/// Projections that give their input's rows as they are dropped (a sort's
/// keys taken off above it, once the sort is gone and narrowing has run,
/// leave one): each would copy every row.
fn same_rows(plan: Plan) -> Plan {
    match map_children(plan, &same_rows) {
        Plan::Project { input, exprs }
            if exprs.len() == input.width()
                && exprs.iter().enumerate().all(|(i, e)| *e == Expr::Column(i)) =>
        {
            *input
        }
        p => p,
    }
}

/// Sorts their inputs already satisfy dropped (**provided order**): a
/// table is read in primary-key order — a row table's tree, a column
/// table's segments and delta merged, a parallel scan's morsels in key
/// order —, through filters and projections that keep it, so `ORDER BY`
/// a prefix of the key, ascending (its columns are never `NULL`), is that
/// order already; ties, in a stable sort in input order, are too.  Once
/// the whole key is ordered on, the keys after it are (it is unique).
/// (PostgreSQL's pathkeys, an index scan for an ordering; DuckDB's and
/// Umbra's order properties.)
fn sorted(plan: Plan) -> Plan {
    match map_children(plan, &sorted) {
        Plan::Sort { input, keys } if provides(&input, &keys) => *input,
        p => p,
    }
}

fn provides(input: &Plan, keys: &[crate::plan::SortKey]) -> bool {
    let Some((order, unique)) = key_order(input) else {
        return false;
    };
    for (i, k) in keys.iter().enumerate() {
        if i == order.len() {
            return unique;
        }
        if k.desc || k.expr != Expr::Column(order[i]) {
            return false;
        }
    }
    true
}

/// The output columns `plan`'s rows are in the order of (each ascending,
/// none `NULL`), and whether they are a key: `None` if none.
fn key_order(plan: &Plan) -> Option<(Vec<usize>, bool)> {
    use crate::types::DataType as T;
    match plan {
        Plan::Scan { table, access, .. } => {
            let primary = matches!(access, Access::Full | Access::Key { index: None, .. });
            // Kinds whose key bytes order as `ORDER BY` does.
            let ordered = |c: &usize| {
                matches!(
                    table.columns[*c].ty,
                    T::Bool
                        | T::SmallInt
                        | T::Integer
                        | T::BigInt
                        | T::Real
                        | T::Float
                        | T::Text
                        | T::Bytea
                        | T::Char
                        | T::Numeric
                        | T::Date
                        | T::Time
                        | T::Timestamp
                        | T::TimestampTz
                )
            };
            (primary
                && table.partition_by.is_none()
                && !table.primary_key.is_empty()
                && table.primary_key.iter().all(ordered))
            .then(|| (table.primary_key.clone(), true))
        }
        Plan::Filter { input, .. } | Plan::Secure { input, .. } => key_order(input),
        Plan::Project { input, exprs } => {
            let (order, unique) = key_order(input)?;
            let mut out = Vec::new();
            for c in &order {
                match exprs.iter().position(|e| *e == Expr::Column(*c)) {
                    Some(j) => out.push(j),
                    None => return (!out.is_empty()).then_some((out, false)),
                }
            }
            Some((out, unique))
        }
        _ => None,
    }
}

/// A condition's conjuncts, the conjuncts every branch of an `OR` among
/// them has taken out of it: `(a AND b) OR (a AND c)` gives `a` and
/// `b OR c` (equal under SQL's three truth values), so a join condition
/// in each branch joins.
fn split(e: Expr, out: &mut Vec<Expr>) {
    let mut parts = Vec::new();
    conjuncts(e, &mut parts);
    for p in parts {
        factor(p, out);
    }
}

fn disjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Binary(BinaryOp::Or, l, r) => {
            disjuncts(*l, out);
            disjuncts(*r, out);
        }
        e => out.push(e),
    }
}

fn factor(e: Expr, out: &mut Vec<Expr>) {
    let mut branches = Vec::new();
    disjuncts(e.clone(), &mut branches);
    if branches.len() < 2 {
        out.push(e);
        return;
    }
    let sets: Vec<Vec<Expr>> = branches
        .into_iter()
        .map(|b| {
            let mut c = Vec::new();
            conjuncts(b, &mut c);
            c
        })
        .collect();
    // (Taken out, an expression runs once instead of in each branch: not
    // for volatile calls or subqueries.)
    let mut common: Vec<Expr> = Vec::new();
    for c in &sets[0] {
        if !common.contains(c)
            && crate::vector::parallel_safe(c)
            && sets[1..].iter().all(|s| s.contains(c))
        {
            common.push(c.clone());
        }
    }
    if common.is_empty() {
        out.push(e);
        return;
    }
    let rests: Vec<Option<Expr>> = sets
        .into_iter()
        .map(|s| conjunction(s.into_iter().filter(|c| !common.contains(c)).collect()))
        .collect();
    out.extend(common);
    // A branch of only the common conjuncts: the OR holds whenever they do.
    if rests.iter().all(Option::is_some) {
        let mut rests = rests.into_iter().flatten();
        let first = rests.next().expect("two or more");
        out.push(rests.fold(first, |a, b| {
            Expr::Binary(BinaryOp::Or, Box::new(a), Box::new(b))
        }));
    }
}

fn push(plan: Plan, st: &Stats) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => {
            let mut preds = Vec::new();
            split(predicate, &mut preds);
            push_into(*input, preds, st)
        }
        Plan::Scan { .. } | Plan::Join { .. } | Plan::Secure { .. } | Plan::Append { .. } => {
            push_into(plan, vec![], st)
        }
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
        // (A filter above stays above: a window reads every row of its
        // partition.)
        Plan::Window { input, funcs } => Plan::Window {
            input: Box::new(push(*input, st)),
            funcs,
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
        | Plan::Past { .. }
        | Plan::Recursive { .. }
        | Plan::WorkTable { .. }
        | Plan::Function { .. }
        | Plan::View { .. }) => p,
    }
}

/// Whether `e` reads nothing that varies while a plan runs (no parameter,
/// enclosing row, subquery or volatile call): it can be evaluated now.
pub(crate) fn constant(e: &Expr) -> bool {
    let mut c = !e.has_subquery();
    e.walk(&mut |x| match x {
        Expr::Param(_) | Expr::Outer { .. } | Expr::Column(_) => c = false,
        Expr::Call(f, _) if f.volatility() != crate::ext::Volatility::Immutable => c = false,
        _ => {}
    });
    c
}

/// A constant condition's truth now (`None`: it does not evaluate).
fn fold(e: &Expr, st: &Stats) -> Option<bool> {
    let reg = crate::ext::Registry::shared_builtin();
    let ctx = crate::expr::Ctx::new(st.txn(), &reg, &[]);
    let env = crate::exec::env(&[], &ctx, None);
    match e.eval(&env).ok()? {
        crate::Value::Bool(b) => Some(b),
        crate::Value::Null => Some(false),
        _ => None,
    }
}

/// `p` with its comparisons' constant operands evaluated now
/// (`d < '1993-07-01'::date + '3 mons'::interval` becomes a comparison with
/// a value): estimated from the statistics, compared with a segment's
/// least and greatest, and computed once rather than per row.
fn folded(p: Expr, st: &Stats) -> Expr {
    let value = |e: Box<Expr>| -> Box<Expr> {
        if matches!(*e, Expr::Literal(_)) || !constant(&e) {
            return e;
        }
        let reg = crate::ext::Registry::shared_builtin();
        let ctx = crate::expr::Ctx::new(st.txn(), &reg, &[]);
        match e.eval(&crate::exec::env(&[], &ctx, None)) {
            Ok(v) => Box::new(Expr::Literal(v)),
            Err(_) => e,
        }
    };
    match p {
        Expr::Binary(op @ (BinaryOp::And | BinaryOp::Or), a, b) => {
            Expr::Binary(op, Box::new(folded(*a, st)), Box::new(folded(*b, st)))
        }
        Expr::Unary(crate::ast::UnaryOp::Not, e) => {
            Expr::Unary(crate::ast::UnaryOp::Not, Box::new(folded(*e, st)))
        }
        Expr::Binary(
            op @ (BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq),
            a,
            b,
        ) => Expr::Binary(op, value(a), value(b)),
        other => other,
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
            let preds: Vec<Expr> = preds.into_iter().map(|p| folded(p, st)).collect();
            let mut preds = preds;
            // (Pushed again — joins reordered, an OR's implications made
            // again —, a conjunct is there once.)
            let mut seen = Vec::with_capacity(preds.len());
            preds.retain(|p| {
                let new = !seen.contains(p);
                if new {
                    seen.push(p.clone());
                }
                new
            });
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
        // A partitioned table: the predicates into each partition, and the
        // partitions they rule out left out — now when the guard is a
        // constant, else when the plan runs.
        Plan::Append {
            inputs,
            spec,
            parts,
            ..
        } => {
            let (mut keep, mut kept_parts, mut kept_guards) = (Vec::new(), Vec::new(), Vec::new());
            for (input, (bound, name)) in inputs.into_iter().zip(parts) {
                let guard = crate::partition::guard(&spec, &bound, &preds);
                let guard = match guard {
                    Some(g) if constant(&g) => match fold(&g, st) {
                        Some(false) => continue,
                        Some(true) => None,
                        None => Some(g),
                    },
                    g => g,
                };
                keep.push(push_into(input, preds.clone(), st));
                kept_parts.push((bound, name));
                kept_guards.push(guard);
            }
            Plan::Append {
                inputs: keep,
                spec,
                parts: kept_parts,
                guards: kept_guards,
            }
        }
        // A barrier: the statement's predicates stay above it.  Those that
        // bound a column by a value free of the row may still narrow the
        // scan's key range — evaluated once, on no row — and the policy's
        // conjuncts may too.
        Plan::Secure { input, policy } => match *input {
            Plan::Scan { table, filter, .. } => {
                let mut pol = Vec::new();
                conjuncts(policy, &mut pol);
                if let Some(f) = filter {
                    conjuncts(f, &mut pol);
                }
                let mut bounds = pol.clone();
                bounds.extend(preds.iter().filter(|p| column_bound(p).is_some()).cloned());
                let access = choose_access(&table, &bounds, Some(st));
                let estimate = st.of(&table).map(|_| {
                    let sel =
                        conjunction(bounds.clone()).map_or(1.0, |p| st.selectivity(&table, &p));
                    (st.rows(&table) * sel).max(1.0)
                });
                let policy =
                    conjunction(pol).unwrap_or(Expr::Literal(crate::types::Value::Bool(true)));
                filtered(
                    Plan::Secure {
                        input: Box::new(Plan::Scan {
                            table,
                            access,
                            filter: None,
                            estimate,
                        }),
                        policy,
                    },
                    preds,
                )
            }
            other => filtered(
                Plan::Secure {
                    input: Box::new(push(other, st)),
                    policy,
                },
                preds,
            ),
        },
        Plan::Project { input, exprs } => {
            // Through the projection, by substituting its expressions —
            // unless they hold subqueries (they would run more often).
            let (through, stay): (Vec<Expr>, Vec<Expr>) = preds
                .into_iter()
                .partition(|p| p.reads().iter().all(|&c| !exprs[c].has_subquery()));
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
            algo,
        } => {
            let lw = left.width();
            let (mut lp, mut rp, mut onp, mut above) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let side = |e: &Expr| -> (bool, bool) {
                let cols = e.reads();
                (cols.iter().all(|&c| c < lw), cols.iter().all(|&c| c >= lw))
            };
            for p in preds {
                match (side(&p), kind) {
                    // Both sides of a full join are preserved: nothing
                    // moves below it.
                    (_, JoinType::Full) => above.push(p),
                    // A semi or anti join only drops rows: a correlated
                    // subquery runs for fewer of them after it (one that
                    // is not runs once anyway).
                    (_, JoinType::Semi | JoinType::Anti) if correlated(&p) => above.push(p),
                    ((true, _), _) => lp.push(p),
                    ((_, true), JoinType::Inner) => rp.push(shift(&p, -(lw as isize))),
                    (_, JoinType::Inner) => onp.push(p),
                    (_, JoinType::Left | JoinType::Semi | JoinType::Anti) => above.push(p),
                }
            }
            if let Some(on) = on {
                let mut parts = Vec::new();
                split(on, &mut parts);
                for p in parts {
                    match (side(&p), kind) {
                        (_, JoinType::Full) => onp.push(p),
                        // Only inner and semi joins may move ON conjuncts
                        // on the preserved side out of the condition.
                        ((true, false), JoinType::Inner | JoinType::Semi) => lp.push(p),
                        ((false, true), _) => rp.push(shift(&p, -(lw as isize))),
                        _ => onp.push(p),
                    }
                }
            }
            // What an OR across both sides implies of each: the OR of each
            // branch's conjuncts on that side, when every branch has some
            // (`(a1 AND b1) OR (a2 AND b2)` implies `a1 OR a2` of the left)
            // — pushed down besides the OR, which stays.
            if kind == JoinType::Inner {
                for p in onp.iter().chain(&above) {
                    if let Some(l) = implied(p, &|e| side(e).0) {
                        lp.push(l);
                    }
                    if let Some(r) = implied(p, &|e| side(e).1) {
                        rp.push(shift(&r, -(lw as isize)));
                    }
                }
            }
            let left = push_into(*left, lp, st);
            let right = push_into(*right, rp, st);
            // A lateral item's join runs it for each left row.
            let algo = match algo {
                JoinAlgo::Lookup => JoinAlgo::Lookup,
                _ => hash_keys(&onp, lw),
            };
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

/// Whether `e` holds a subquery that refers to the row it runs for.
fn correlated(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Subquery(s) = x {
            found |= s.correlated;
        }
    });
    found
}

/// What the OR `p` implies of one side (`on_side` of a conjunct): the OR
/// of each branch's conjuncts on it, when every branch has one (and they
/// are safe to evaluate on their own: no volatile call, no subquery).
fn implied(p: &Expr, on_side: &dyn Fn(&Expr) -> bool) -> Option<Expr> {
    let mut branches = Vec::new();
    disjuncts(p.clone(), &mut branches);
    if branches.len() < 2 {
        return None;
    }
    let mut parts = Vec::with_capacity(branches.len());
    for b in branches {
        let mut cs = Vec::new();
        conjuncts(b, &mut cs);
        let mine: Vec<Expr> = cs
            .into_iter()
            .filter(|c| on_side(c) && !c.columns().is_empty() && crate::vector::parallel_safe(c))
            .collect();
        parts.push(conjunction(mine)?);
    }
    let mut parts = parts.into_iter();
    let first = parts.next()?;
    Some(parts.fold(first, |a, b| {
        Expr::Binary(BinaryOp::Or, Box::new(a), Box::new(b))
    }))
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
            build_left: false,
            adaptive: None,
        }
    }
}

/// `Column(c) op e` with `e` free of this row's columns (and of
/// subqueries): `(c, op, e)` with `op` as if the column were on the left.
pub(crate) fn column_bound(p: &Expr) -> Option<(usize, BinaryOp, Expr)> {
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

/// `key op e` with `e` free of this row's columns (and of subqueries):
/// `(op, e)` with `op` as if `key` were on the left — `key` a column, or
/// an index's expression part, matched as bound (PLAN.md §M14.15 J4).
fn key_bound(p: &Expr, key: &Expr) -> Option<(BinaryOp, Expr)> {
    if let Expr::Column(c) = key {
        return column_bound(p)
            .filter(|(col, _, _)| col == c)
            .map(|(_, op, e)| (op, e));
    }
    let Expr::Binary(op, a, b) = p else {
        return None;
    };
    if !matches!(
        op,
        BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
    ) {
        return None;
    }
    let free = |e: &Expr| e.columns().is_empty() && !e.has_subquery();
    let flip = |op: BinaryOp| match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        op => op,
    };
    if **a == *key && free(b) {
        Some((*op, (**b).clone()))
    } else if **b == *key && free(a) {
        Some((flip(*op), (**a).clone()))
    } else {
        None
    }
}

/// The access path for a scan with filter conjuncts `preds`.  With
/// statistics: the cheapest of a full scan and each usable index.
/// Without: the longest equality prefix, then a range on the next key
/// part, the primary key winning ties.  An index's key part is a column
/// or an expression (with statistics only: they bind it,
/// [`Stats::key_parts`]).
pub fn choose_access(table: &Table, preds: &[Expr], st: Option<&Stats>) -> Access {
    let eq = |k: &Expr| {
        preds
            .iter()
            .filter_map(|p| key_bound(p, k))
            .find(|(op, _)| *op == BinaryOp::Eq)
            .map(|(_, e)| e)
    };
    let columns = |cols: &[usize]| cols.iter().map(|&c| Expr::Column(c)).collect::<Vec<_>>();
    let candidates = std::iter::once((None, columns(&table.primary_key))).chain(
        table
            .indexes
            .iter()
            .enumerate()
            // An index in a taught ordering holds mappings, not the
            // columns' values, and a registered method's is searched its
            // own way: no access path for predicates on them.
            .filter(|(_, ix)| ix.method.is_none() && !ix.mapped())
            .filter_map(|(i, ix)| {
                if !ix.has_exprs() {
                    return Some((Some(i), columns(&ix.columns)));
                }
                Some((Some(i), st?.key_parts(table, Some(i))?))
            }),
    );
    let analyzed = st.filter(|s| s.of(table).is_some());
    let rows = analyzed.map_or(0.0, |s| s.rows(table));
    // (score or negative cost, access)
    let mut best: Option<(f64, Access)> = None;
    for (index, keys) in candidates {
        if keys.is_empty() {
            continue; // a hidden row id
        }
        let prefix: Vec<Expr> = keys.iter().map_while(&eq).collect();
        let (mut lower, mut upper) = (Bound::Unbounded, Bound::Unbounded);
        let mut used: Vec<Expr> = prefix
            .iter()
            .zip(&keys)
            .map(|(e, k)| bin(BinaryOp::Eq, k, e))
            .collect();
        if let Some(next) = keys.get(prefix.len()) {
            for (op, e) in preds.iter().filter_map(|p| key_bound(p, next)) {
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
                    used.push(bin(op, next, &e));
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
                let full = prefix.len() == keys.len();
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

fn bin(op: BinaryOp, k: &Expr, e: &Expr) -> Expr {
    Expr::Binary(op, Box::new(k.clone()), Box::new(e.clone()))
}

// ------------------------------------------------------------- join order

/// The table column a plan's output column comes from, if any.
pub(crate) fn origin(plan: &Plan, col: usize) -> Option<(&Table, usize)> {
    match plan {
        Plan::Scan { table, .. } => Some((table, col)),
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input } => origin(input, col),
        Plan::Window { input, .. } if col < input.width() => origin(input, col),
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
pub(crate) fn estimate(plan: &Plan, st: &Stats) -> f64 {
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
        Plan::Append { inputs, .. } => inputs.iter().map(|p| estimate(p, st)).sum(),
        Plan::Cte { plan, .. } | Plan::Past { plan, .. } => estimate(plan, st),
        Plan::Recursive { base, .. } => estimate(base, st) * 10.0,
        Plan::WorkTable { .. } => 10.0,
        Plan::Function { .. } => 100.0,
        Plan::View { .. } => 100.0,
        Plan::Filter { input, .. } => estimate(input, st) * 0.33,
        Plan::Secure { input, .. } => estimate(input, st) * 0.5,
        Plan::Project { input, .. } | Plan::Sort { input, .. } | Plan::Window { input, .. } => {
            estimate(input, st)
        }
        Plan::Distinct { input } => {
            let rows = estimate(input, st);
            let all: Vec<Expr> = (0..input.width()).map(Expr::Column).collect();
            groups_of(input, &all, st).map_or(rows * 0.5, |g| g.min(rows).max(1.0))
        }
        Plan::Limit { input, limit, .. } => match limit {
            Some(Expr::Literal(crate::Value::Int(n))) => estimate(input, st).min(*n as f64),
            _ => estimate(input, st),
        },
        Plan::Aggregate { input, group, .. } => {
            if group.is_empty() {
                1.0
            } else {
                let rows = estimate(input, st);
                groups_of(input, group, st)
                    .map_or(rows * 0.1, |g| g.min(rows))
                    .max(1.0)
            }
        }
        Plan::Join {
            left,
            right,
            kind,
            algo: JoinAlgo::Lookup,
            ..
        } => {
            // The right side's estimate is a lookup's: per left row.
            let (l, per) = (estimate(left, st), estimate(right, st));
            match kind {
                JoinType::Inner => l * per,
                JoinType::Left => (l * per).max(l),
                JoinType::Semi | JoinType::Anti | JoinType::Full => l,
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
                let origin_of = |c: usize| {
                    if c < lw {
                        origin(left, c)
                    } else {
                        origin(right, c - lw)
                    }
                };
                let each: Vec<f64> = parts
                    .iter()
                    .map(|p| join_selectivity(p, origin_of, st))
                    .collect();
                let side = |c: usize| usize::from(c >= lw);
                for s in keyed_selectivity(&parts, each, &side, &origin_of, st) {
                    sel *= s;
                }
            }
            let out = l * r * sel;
            match kind {
                JoinType::Inner => out,
                JoinType::Left => out.max(l),
                JoinType::Full => out.max(l).max(r),
                JoinType::Semi => out.min(l).max(1.0),
                JoinType::Anti => l,
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

/// Equalities between the same two inputs whose columns on one side cover
/// a key of its table (the primary key, a unique index): each row of the
/// other meets at most one of its rows, so together they select
/// 1 / its rows — not the product of their selectivities, which takes
/// them for independent (and a join on a two-column key for none).  Else,
/// where a side's columns were counted together (`Stats::group_distinct`),
/// they select 1 / the combinations of the side with more.
fn keyed_selectivity<'p>(
    conds: &[Expr],
    mut sel: Vec<f64>,
    leaf_of: &dyn Fn(usize) -> usize,
    origin_of: &dyn Fn(usize) -> Option<(&'p Table, usize)>,
    st: &Stats,
) -> Vec<f64> {
    let columns = |c: &Expr| match c {
        Expr::Binary(BinaryOp::Eq, a, b) => match (&**a, &**b) {
            (Expr::Column(x), Expr::Column(y)) if leaf_of(*x) != leaf_of(*y) => Some((*x, *y)),
            _ => None,
        },
        _ => None,
    };
    let mut groups: std::collections::BTreeMap<(usize, usize), Vec<usize>> = Default::default();
    for (k, c) in conds.iter().enumerate() {
        if let Some((x, y)) = columns(c) {
            let (a, b) = (leaf_of(x), leaf_of(y));
            groups.entry((a.min(b), a.max(b))).or_default().push(k);
        }
    }
    'groups: for ((a, b), ks) in groups {
        if ks.len() < 2 {
            continue;
        }
        let mut sides = Vec::new();
        for side in [a, b] {
            let mut table = None;
            let mut cols = Vec::new();
            for &k in &ks {
                let (x, y) = columns(&conds[k]).expect("an equality");
                let c = if leaf_of(x) == side { x } else { y };
                match origin_of(c) {
                    Some((t, col)) if table.is_none_or(|u: &Table| u.id == t.id) => {
                        table = Some(t);
                        cols.push(col);
                    }
                    _ => {
                        table = None;
                        break;
                    }
                }
            }
            let Some(t) = table else {
                sides.push(None);
                continue;
            };
            let covers = |key: &[usize]| !key.is_empty() && key.iter().all(|c| cols.contains(c));
            if covers(&t.primary_key)
                || t.indexes
                    .iter()
                    .any(|i| i.unique && i.key_columns().is_some_and(covers))
            {
                let product: f64 = ks.iter().map(|&k| sel[k]).product();
                let keyed = 1.0 / st.rows(t).max(1.0);
                if keyed > product {
                    sel[ks[0]] = keyed;
                    for &k in &ks[1..] {
                        sel[k] = 1.0;
                    }
                }
                continue 'groups;
            }
            sides.push(Some((t, cols)));
        }
        let combos: Vec<(f64, bool)> = sides
            .iter()
            .map(|s| {
                s.as_ref()
                    .map_or((1.0, false), |(t, cols)| st.group_distinct(t, cols))
            })
            .collect();
        if combos.iter().any(|c| c.1) {
            sel[ks[0]] = 1.0 / combos[0].0.max(combos[1].0).max(1.0);
            for &k in &ks[1..] {
                sel[k] = 1.0;
            }
        }
    }
    sel
}

/// The groups `exprs` (over `input`'s columns) make, if all are columns of
/// analyzed tables: their combinations per table, multiplied.
/// The groups `exprs` (columns) make of `input`'s rows, by the statistics.
pub(crate) fn groups_of_exprs(input: &Plan, exprs: &[Expr], st: &Stats) -> Option<f64> {
    groups_of(input, exprs, st)
}

fn groups_of(input: &Plan, exprs: &[Expr], st: &Stats) -> Option<f64> {
    let mut per: Vec<(&Table, Vec<usize>)> = Vec::new();
    for e in exprs {
        let Expr::Column(c) = e else { return None };
        let (t, col) = origin(input, *c)?;
        st.of(t)?;
        match per.iter_mut().find(|(u, _)| u.id == t.id) {
            Some((_, cols)) => cols.push(col),
            None => per.push((t, vec![col])),
        }
    }
    Some(
        per.iter()
            .map(|(t, cols)| st.group_distinct(t, cols).0)
            .product(),
    )
}

/// Each semi and anti join moved down among the (ordered) inner joins under
/// it to where it meets the fewest rows (`unnest::place`), hashed on its
/// equalities there.
fn sink(plan: Plan, st: &Stats) -> Plan {
    match map_children(plan, &|c| sink(c, st)) {
        Plan::Join {
            left,
            right,
            kind: kind @ (JoinType::Semi | JoinType::Anti),
            on: Some(on),
            ..
        } => {
            let known = analyzed(&left, st);
            let rows = |p: &Plan| known.then(|| estimate(p, st));
            hashed(crate::unnest::place(*left, *right, on, kind, &rows))
        }
        other => other,
    }
}

/// `plan` with its semi and anti joins without an algorithm yet (as
/// `unnest::place` makes them) hashed on their equalities.
fn hashed(plan: Plan) -> Plan {
    match map_children(plan, &hashed) {
        Plan::Join {
            left,
            right,
            kind: kind @ (JoinType::Semi | JoinType::Anti),
            on: Some(on),
            algo: JoinAlgo::NestedLoop,
        } => {
            let mut parts = Vec::new();
            conjuncts(on.clone(), &mut parts);
            let algo = hash_keys(&parts, left.width());
            Plan::Join {
                left,
                right,
                kind,
                on: Some(on),
                algo,
            }
        }
        other => other,
    }
}

/// An index lookup costs about this many rows of a scan, each row a
/// secondary index finds this many more (its fetch from the primary
/// index).  Measured on TPC-H's row tables, one thread (§M14.12 V3): a
/// lookup into `orders` by its key 2.9 µs, a row `lineitem`'s index on
/// `l_partkey` finds 5.8 µs, against 0.07–0.1 µs a hash join's row.
const LOOKUP_ROWS: f64 = 40.0;
const FETCH_ROWS: f64 = 60.0;
/// A column table's lookup against a row table's, and its scan's rows.
const COLUMN_LOOKUP: f64 = 1.2;
const COLUMN_SCAN: f64 = 0.25;

/// Hash joins whose right side is a table's scan, where an index on the
/// join keys finds a left row's matches for less than reading the whole
/// table: index nested loops (`JoinAlgo::Lookup`), the right side a scan by
/// that index of the left row's values.  Analyzed tables only.
fn lookups(plan: Plan, st: &Stats) -> Plan {
    match map_children(plan, &|c| lookups(c, st)) {
        Plan::Join {
            left,
            right,
            kind,
            on,
            algo:
                JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    ..
                },
        } => {
            let existence = matches!(kind, JoinType::Semi | JoinType::Anti);
            let lookup = (kind != JoinType::Full)
                .then(|| lookup_side(&left, &right, &left_keys, &right_keys, existence, st))
                .flatten();
            // An inner join may look the left side up instead (the larger,
            // which the reordering made the probe side), the columns put
            // back in order above.
            let swapped = (kind == JoinType::Inner && lookup.is_none())
                .then(|| lookup_side(&right, &left, &right_keys, &left_keys, false, st))
                .flatten();
            match (lookup, swapped) {
                (Some(right), _) => Plan::Join {
                    left,
                    right: Box::new(right),
                    kind,
                    on,
                    algo: JoinAlgo::Lookup,
                },
                (None, Some(looked_up)) => {
                    let (lw, rw) = (left.width(), right.width());
                    let back = |c: usize| Expr::Column(if c < lw { c + rw } else { c - lw });
                    Plan::Project {
                        input: Box::new(Plan::Join {
                            left: right,
                            right: Box::new(looked_up),
                            kind,
                            on: on.map(|e| e.map_columns(&back)),
                            algo: JoinAlgo::Lookup,
                        }),
                        exprs: (0..lw + rw).map(back).collect(),
                    }
                }
                (None, None) => {
                    // A semi or anti join hashes the smaller side.
                    let build_left = existence
                        && analyzed(&left, st)
                        && analyzed(&right, st)
                        && 2.0 * estimate(&left, st) < estimate(&right, st);
                    Plan::Join {
                        left,
                        right,
                        kind,
                        on,
                        algo: JoinAlgo::Hash {
                            left_keys,
                            right_keys,
                            build_left,
                            adaptive: None,
                        },
                    }
                }
            }
        }
        other => other,
    }
}

/// `right` (a scan, under a projection of columns) as a scan by an index
/// of `left_keys`' values — when cheaper than hashing it all.
fn lookup_side(
    left: &Plan,
    right: &Plan,
    left_keys: &[Expr],
    right_keys: &[Expr],
    existence: bool,
    st: &Stats,
) -> Option<Plan> {
    let (plan, costs) = lookup_plan(right, left_keys, right_keys, existence, st)?;
    let probes = estimate(left, st).max(1.0);
    (costs.lookup(probes) < costs.hash(probes)).then_some(plan)
}

/// What looking a table up costs against hashing it, by the rows that look
/// it up ([`lookup_plan`]).
struct LookupCosts {
    /// A lookup's cost.
    each: f64,
    /// Hashing the table, its rows read and kept.
    table: f64,
}

impl LookupCosts {
    fn lookup(&self, probes: f64) -> f64 {
        self.each * probes
    }

    fn hash(&self, probes: f64) -> f64 {
        self.table + 1.5 * probes
    }

    /// The most rows looking it up costs less for (none: 0).
    fn below(&self) -> usize {
        if self.each <= 1.5 {
            return 0;
        }
        (self.table / (self.each - 1.5)).floor() as usize
    }
}

/// `right` (a scan, under a projection of columns) as a scan by an index
/// of `left_keys`' values (the left row `Outer` at depth 1), and what it
/// costs against hashing it.
fn lookup_plan(
    right: &Plan,
    left_keys: &[Expr],
    right_keys: &[Expr],
    existence: bool,
    st: &Stats,
) -> Option<(Plan, LookupCosts)> {
    // Its references to enclosing rows one level further: the left row is
    // the nearest now.
    let mut right = right.clone();
    right.deepen(1, 1);
    let (scan, cols) = match right {
        Plan::Project { input, exprs } if exprs.iter().all(|e| matches!(e, Expr::Column(_))) => {
            (*input, Some(exprs))
        }
        p => (p, None),
    };
    let Plan::Scan { table, filter, .. } = scan else {
        return None;
    };
    st.of(&table)?;

    let column = |e: &Expr| match (e, &cols) {
        (Expr::Column(c), None) => Some(*c),
        (Expr::Column(c), Some(x)) => match x.get(*c) {
            Some(Expr::Column(i)) => Some(*i),
            _ => None,
        },
        _ => None,
    };
    let mut preds = Vec::new();
    if let Some(f) = filter {
        conjuncts(f, &mut preds);
    }
    let local = conjunction(preds.clone());
    let mut keys = Vec::new();
    for (l, r) in left_keys.iter().zip(right_keys) {
        if l.has_subquery() {
            return None;
        }
        keys.push(Expr::Binary(
            BinaryOp::Eq,
            Box::new(Expr::Column(column(r)?)),
            Box::new(l.lifted(1)),
        ));
    }
    preds.extend(keys.iter().cloned());
    let access = choose_access(&table, &preds, Some(st));
    let by_row = |e: &Expr| {
        let mut found = false;
        e.walk(&mut |x| found |= matches!(x, Expr::Outer { depth: 1, .. }));
        found
    };
    let secondary = match &access {
        Access::Key { index, prefix, .. } if prefix.iter().any(by_row) => index.is_some(),
        _ => return None,
    };
    // A lookup reads the rows of one key value — all of them, the filter
    // only drops some after — and gives those the filter keeps.
    let rows = st.rows(&table).max(1.0);
    let read = conjunction(keys)
        .map_or(rows, |p| rows * st.selectivity(&table, &p))
        .max(1.0);
    let matches = conjunction(preds.clone())
        .map_or(read, |p| rows * st.selectivity(&table, &p))
        .max(1.0);
    // A semi or anti join's lookup stops at its first match.
    let read = if existence {
        (read / matches).clamp(1.0, read)
    } else {
        read
    };
    // A column table scans for less (its batches, the columns read) and
    // looks a row up for more (segments' columns read).
    let (look, scan) = if table.storage == crate::storage::ROW {
        (1.0, 1.0)
    } else {
        (COLUMN_LOOKUP, COLUMN_SCAN)
    };
    let each = look * (LOOKUP_ROWS + read * if secondary { FETCH_ROWS } else { 1.0 });
    let filtered = local.map_or(rows, |p| rows * st.selectivity(&table, &p));
    let costs = LookupCosts {
        each,
        table: scan * (rows + filtered),
    };
    let scan = Plan::Scan {
        table,
        access,
        filter: conjunction(preds),
        estimate: Some(matches),
    };
    let plan = match cols {
        Some(exprs) => Plan::Project {
            input: Box::new(scan),
            exprs,
        },
        None => scan,
    };
    Some((plan, costs))
}

fn analyzed(plan: &Plan, st: &Stats) -> bool {
    match plan {
        Plan::Scan { table, .. } => st.of(table).is_some(),
        p => p.children().into_iter().any(|c| analyzed(c, st)),
    }
}

/// Reorders chains of inner joins (bottom-up through the plan).
fn reorder(plan: Plan, st: &Stats) -> Plan {
    if !inner_chain(&plan) {
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

pub(crate) fn map_children(plan: Plan, f: &dyn Fn(Plan) -> Plan) -> Plan {
    let b = |p: Box<Plan>| Box::new(f(*p));
    match plan {
        Plan::Filter { input, predicate } => Plan::Filter {
            input: b(input),
            predicate,
        },
        Plan::Secure { input, policy } => Plan::Secure {
            input: b(input),
            policy,
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
        Plan::Window { input, funcs } => Plan::Window {
            input: b(input),
            funcs,
        },
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
        Plan::Append {
            inputs,
            spec,
            parts,
            guards,
        } => Plan::Append {
            inputs: inputs.into_iter().map(f).collect(),
            spec,
            parts,
            guards,
        },
        p @ (Plan::Scan { .. }
        | Plan::Values { .. }
        | Plan::Cte { .. }
        | Plan::Past { .. }
        | Plan::Recursive { .. }
        | Plan::WorkTable { .. }
        | Plan::Function { .. }
        | Plan::View { .. }) => p,
    }
}

/// Whether `plan` is an inner join a chain may reorder: not a lateral
/// item's, whose right side reads the left row (it stays a leaf).
fn inner_chain(plan: &Plan) -> bool {
    matches!(
        plan,
        Plan::Join {
            kind: JoinType::Inner,
            algo: JoinAlgo::NestedLoop | JoinAlgo::Hash { .. },
            ..
        }
    )
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
            algo: JoinAlgo::NestedLoop | JoinAlgo::Hash { .. },
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
        .map(|c| c.reads().iter().fold(0, |m, &col| m | (1 << leaf_of(col))))
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
    let cond_sel = keyed_selectivity(
        &conds,
        cond_sel,
        &leaf_of,
        &|col| {
            let l = leaf_of(col);
            origin(&leaves[l].0, col - leaves[l].1)
        },
        st,
    );
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
    let mut masks: Vec<usize> = (1..=full).filter(|m| m.count_ones() >= 2).collect();
    masks.sort_by_key(|m| m.count_ones());
    // Connected subsets only (DPccp's space, PostgreSQL's
    // `have_relevant_joinclause`): a cross product of two inputs the
    // conditions join only through a third is never planned.  Only a
    // chain the conditions leave in pieces joins them by cross products.
    let mut tree = None;
    for allow_cross in [false, true] {
        let mut best: Vec<Option<(f64, Tree)>> = vec![None; full + 1];
        for i in 0..n {
            best[1 << i] = Some((leaf_rows[i], Tree::Leaf(i)));
        }
        for &mask in &masks {
            let out = rows_of(mask);
            for cross in [false, true] {
                if cross && !allow_cross {
                    break;
                }
                let mut a = (mask - 1) & mask;
                while a > 0 {
                    let b = mask & !a;
                    if let (Some((ca, ta)), Some((cb, tb))) = (&best[a], &best[b]) {
                        let joined = connected(a, b);
                        if joined || cross {
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
        if let Some((_, t)) = best[full].take() {
            tree = Some(t);
            break;
        }
    }
    let tree = tree?;
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
