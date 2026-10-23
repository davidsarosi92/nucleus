//! The executor: plans run as iterators over rows (Volcano style).
//! Joins materialise their right input once; sorts and aggregates their
//! whole input.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::rc::Rc;

use nucleus_kv::prefix_end;

use crate::ast::SetOp;
use crate::catalog::{Table, row_key};
use crate::encoding::{encode_key, encode_key_value};
use crate::error::{Error, Result};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::{Accumulator, CallContext};
use crate::plan::{Access, Agg, JoinAlgo, JoinType, Plan, SortKey, TableFunc};
use crate::types::{DataType, Row, Value};

/// A stream of rows.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// Runs `plan` to completion.
pub fn collect(plan: &Plan, ctx: &Ctx, outer: Option<&Env>) -> Result<Vec<Row>> {
    build(plan, ctx, outer)?.collect()
}

/// The environment a row is evaluated in.
pub fn env<'a>(row: &'a [Value], ctx: &'a Ctx<'a>, outer: Option<&'a Env<'a>>) -> Env<'a> {
    Env { row, outer, ctx }
}

/// Builds the iterator for `plan`.
pub fn build<'a>(plan: &'a Plan, ctx: &'a Ctx<'a>, outer: Option<&'a Env<'a>>) -> Result<Rows<'a>> {
    Ok(match plan {
        Plan::Scan {
            table,
            access,
            filter,
            ..
        } => {
            let rows = scan(table, access, ctx, outer)?;
            match filter {
                None => Box::new(rows.map(|r| r.map(|(_, row)| row))),
                Some(f) => Box::new(rows.filter_map(move |r| match r {
                    Err(e) => Some(Err(e)),
                    Ok((_, row)) => match f.holds(&env(&row, ctx, outer)) {
                        Ok(true) => Some(Ok(row)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    },
                })),
            }
        }
        Plan::Values { rows, .. } => {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let e = env(&[], ctx, outer);
                out.push(r.iter().map(|x| x.eval(&e)).collect::<Result<Row>>());
            }
            Box::new(out.into_iter())
        }
        Plan::Filter { input, predicate }
        | Plan::Secure {
            input,
            policy: predicate,
        } => {
            let rows = build(input, ctx, outer)?;
            Box::new(rows.filter_map(move |r| match r {
                Err(e) => Some(Err(e)),
                Ok(row) => match predicate.holds(&env(&row, ctx, outer)) {
                    Ok(true) => Some(Ok(row)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                },
            }))
        }
        Plan::Project { input, exprs } => {
            let rows = build(input, ctx, outer)?;
            Box::new(rows.map(move |r| {
                let row = r?;
                let e = env(&row, ctx, outer);
                exprs.iter().map(|x| x.eval(&e)).collect()
            }))
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
            algo,
        } => join(left, right, *kind, on.as_ref(), algo, ctx, outer)?,
        Plan::Aggregate { input, group, aggs } => Box::new(
            aggregate(build(input, ctx, outer)?, group, aggs, ctx, outer)?
                .into_iter()
                .map(Ok),
        ),
        Plan::Sort { input, keys } => Box::new(
            sort(build(input, ctx, outer)?, keys, ctx, outer)?
                .into_iter()
                .map(Ok),
        ),
        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            let e = env(&[], ctx, outer);
            let count = |x: &Option<Expr>, what: &str| -> Result<Option<usize>> {
                match x.as_ref().map(|x| x.eval(&e)).transpose()? {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::Int(n)) if n >= 0 => Ok(Some(n as usize)),
                    Some(v) => Err(Error::invalid_parameter(format!(
                        "{what} must not be negative or non-integer: {v}"
                    ))),
                }
            };
            let (limit, offset) = (
                count(limit, "LIMIT")?,
                count(offset, "OFFSET")?.unwrap_or(0),
            );
            let rows = build(input, ctx, outer)?.skip(offset);
            match limit {
                Some(n) => Box::new(rows.take(n)),
                None => Box::new(rows),
            }
        }
        Plan::Distinct { input } => {
            let mut seen = HashSet::new();
            Box::new(build(input, ctx, outer)?.filter(move |r| match r {
                Ok(row) => seen.insert(row.clone()),
                Err(_) => true,
            }))
        }
        Plan::View { view } => Box::new(
            view.rows(&CallContext::new(ctx.txn, ctx.registry))?
                .into_iter()
                .map(Ok),
        ),
        Plan::Function {
            func,
            args,
            ordinality,
            ..
        } => {
            let e = env(&[], ctx, outer);
            let vals = args
                .iter()
                .map(|a| a.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            let mut rows: Vec<Row> = match func {
                TableFunc::Scalar => vec![vec![vals[0].clone()]],
                TableFunc::Call(f) => f.rows(&CallContext::new(ctx.txn, ctx.registry), &vals)?,
            };
            if !matches!(func, TableFunc::Call(f) if f.name() == "generate_series") {
                for _ in &rows {
                    crate::budget::charge_scan()?;
                }
            }
            if *ordinality {
                for (i, r) in rows.iter_mut().enumerate() {
                    r.push(Value::Int(i as i64 + 1));
                }
            }
            Box::new(rows.into_iter().map(Ok))
        }
        Plan::Cte { id, plan, .. } => {
            let cached = ctx.ctes.borrow().get(id).cloned();
            let rows = match cached {
                Some(rows) => rows,
                None => {
                    let rows = Rc::new(collect(plan, ctx, outer)?);
                    ctx.ctes.borrow_mut().insert(*id, Rc::clone(&rows));
                    rows
                }
            };
            shared(rows)
        }
        Plan::WorkTable { id, .. } => {
            let rows = ctx.work.borrow().get(id).cloned().unwrap_or_default();
            shared(rows)
        }
        Plan::Recursive {
            id,
            base,
            step,
            all,
        } => {
            let mut seen = HashSet::new();
            let mut fresh = |rows: Vec<Row>| -> Vec<Row> {
                if *all {
                    rows
                } else {
                    rows.into_iter()
                        .filter(|r| seen.insert(r.clone()))
                        .collect()
                }
            };
            let mut out = fresh(collect(base, ctx, outer)?);
            let mut round = out.clone();
            while !round.is_empty() {
                // (The step reads the work table only outside subqueries
                // and cached CTEs, so no cache holds an earlier round.)
                ctx.work.borrow_mut().insert(*id, Rc::new(round));
                round = fresh(collect(step, ctx, outer)?);
                // Each row a round makes counts as read: an endless
                // recursion runs into the statement's budget.
                for _ in &round {
                    crate::budget::charge_scan()?;
                }
                out.extend(round.iter().cloned());
            }
            ctx.work.borrow_mut().remove(id);
            Box::new(out.into_iter().map(Ok))
        }
        Plan::SetOp {
            op: SetOp::Union,
            left,
            right,
            all,
        } => {
            let rows = build(left, ctx, outer)?.chain(build(right, ctx, outer)?);
            if *all {
                Box::new(rows)
            } else {
                let mut seen = HashSet::new();
                Box::new(rows.filter(move |r| match r {
                    Ok(row) => seen.insert(row.clone()),
                    Err(_) => true,
                }))
            }
        }
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => {
            // How often each right row occurs; left rows are matched
            // against it (multiset semantics for ALL, as in PostgreSQL).
            let mut counts: HashMap<Row, usize> = HashMap::new();
            for r in build(right, ctx, outer)? {
                *counts.entry(r?).or_default() += 1;
            }
            let (op, all) = (*op, *all);
            let mut seen = HashSet::new();
            Box::new(build(left, ctx, outer)?.filter(move |r| {
                let Ok(row) = r else { return true };
                let count = counts.get_mut(row);
                let keep = match (op, count) {
                    (SetOp::Intersect, Some(n)) if *n > 0 => {
                        *n -= 1;
                        true
                    }
                    (SetOp::Intersect, _) => false,
                    (_, Some(n)) if *n > 0 => {
                        *n -= 1;
                        false
                    }
                    (_, Some(_)) => all, // EXCEPT ALL: more on the left
                    (_, None) => true,
                };
                // Without ALL each distinct row appears at most once, and
                // EXCEPT drops a row the right side has at all.
                keep && (all || seen.insert(row.clone()))
            }))
        }
    })
}

/// `generate_series(start, stop [, step])`: numbers, or timestamps with an
/// interval step.  NULL arguments give no rows.
/// `generate_series`'s rows (the built-in's implementation).
pub(crate) fn generate_series(vals: &[Value]) -> Result<Vec<Row>> {
    use crate::ast::BinaryOp;
    use crate::expr::binary;
    if vals.iter().any(Value::is_null) {
        return Ok(Vec::new());
    }
    let (start, stop) = (vals[0].clone(), vals[1].clone());
    let step = match vals.get(2) {
        Some(s) => s.clone(),
        None if start.as_timestamp().is_some() => {
            return Err(Error::undefined_function(
                "generate_series over timestamps needs an interval step",
            ));
        }
        None => Value::Int(1),
    };
    let zero = match &step {
        Value::Interval(i) => i.span() == 0,
        v => v.sql_cmp(&Value::Int(0))? == Some(Ordering::Equal),
    };
    if zero {
        return Err(Error::invalid_parameter("step size cannot equal zero"));
    }
    let up = match &step {
        Value::Interval(i) => i.span() > 0,
        v => v.sql_cmp(&Value::Int(0))? == Some(Ordering::Greater),
    };
    let mut out = Vec::new();
    let mut cur = start;
    loop {
        let o = cur.sql_cmp(&stop)?.expect("not null");
        if (up && o == Ordering::Greater) || (!up && o == Ordering::Less) {
            return Ok(out);
        }
        if out.len() >= 100_000_000 {
            return Err(Error::out_of_range("generate_series: too many rows"));
        }
        // Each row made counts as read (and the statement's time runs).
        crate::budget::charge_scan()?;
        out.push(vec![cur.clone()]);
        cur = match binary(BinaryOp::Add, cur, step.clone()) {
            Ok(v) => v,
            Err(e) if e.code == "22003" || e.code == "22008" => return Ok(out), // past the end of the type
            Err(e) => return Err(e),
        };
    }
}

/// Rows shared behind an `Rc`, cloned as they are read.
fn shared<'a>(rows: Rc<Vec<Row>>) -> Rows<'a> {
    Box::new((0..rows.len()).map(move |i| Ok(rows[i].clone())))
}

/// `v` as a key bound in a column of type `t`.  A bound of the wrong type
/// would encode in the wrong place: then `None`, and the caller reads more
/// (the filter decides), never less.  Exact conversions only: a value that
/// compares equal to the column's values the way its converted form does.
fn key_bound(v: Value, t: DataType) -> Option<Value> {
    Some(match (v, t) {
        (v, t) if v.is_null() || v.data_type() == Some(t) => v,
        // Every integer width has the same key encoding; so do inet and cidr.
        (v @ Value::Int(_), t) if t.is_integer() => v,
        (v @ (Value::Inet(_) | Value::Cidr(_)), DataType::Inet | DataType::Cidr) => v,
        (Value::Int(i), DataType::Float) => Value::Float(i as f64),
        (Value::Int(i), DataType::Numeric) => Value::Numeric(crate::decimal::Decimal::from_i64(i)),
        // numeric against float compares as floats.
        (Value::Numeric(d), DataType::Float) => Value::Float(d.to_f64()),
        (v @ (Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_)), t)
            if t.is_datetime() && t != DataType::Date =>
        {
            let ts = v.as_timestamp()?;
            if t == DataType::Timestamp {
                Value::Timestamp(ts)
            } else {
                Value::TimestampTz(ts)
            }
        }
        _ => return None,
    })
}

/// The KV key range an access path reads: `(start, end, index)`.
fn key_range(
    table: &Table,
    access: &Access,
    e: &Env,
) -> Result<Option<(Vec<u8>, Option<Vec<u8>>, Option<usize>)>> {
    let Access::Key {
        index,
        prefix,
        lower,
        upper,
    } = access
    else {
        let p = table.index_prefix(0);
        let end = prefix_end(&p);
        return Ok(Some((p, end, None)));
    };
    let columns: &[usize] = match index {
        None => &table.primary_key,
        Some(i) => &table.indexes[*i].columns,
    };
    let id = index.map_or(0, |i| table.indexes[i].id);
    let mut key = table.index_prefix(id);
    let ty = |pos: usize| columns.get(pos).map(|c| table.columns[*c].ty);
    let coerce = |v: Value, pos: usize| key_bound(v, ty(pos)?);
    let mut usable = true;
    for (pos, p) in prefix.iter().enumerate() {
        let v = p.eval(e)?;
        if v.is_null() {
            return Ok(None); // `col = NULL` matches nothing
        }
        let Some(v) = coerce(v, pos) else {
            usable = false;
            break;
        };
        encode_key_value(&mut key, &v);
    }
    let base_end = prefix_end(&key);
    if !usable {
        return Ok(Some((key.clone(), base_end, *index)));
    }
    let pos = prefix.len();
    let bound_key = |b: &Expr| -> Result<Option<Vec<u8>>> {
        let v = b.eval(e)?;
        let Some(v) = coerce(v, pos).filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let mut k = key.clone();
        encode_key_value(&mut k, &v);
        Ok(Some(k))
    };
    let start = match lower {
        Bound::Unbounded => key.clone(),
        Bound::Included(b) => bound_key(b)?.unwrap_or_else(|| key.clone()),
        Bound::Excluded(b) => {
            bound_key(b)?.map_or_else(|| key.clone(), |k| prefix_end(&k).unwrap_or(k))
        }
    };
    let end = match upper {
        Bound::Unbounded => base_end,
        Bound::Included(b) => match bound_key(b)? {
            Some(k) => prefix_end(&k),
            None => base_end,
        },
        Bound::Excluded(b) => bound_key(b)?.or(base_end),
    };
    Ok(Some((start, end, *index)))
}

/// A table's rows through an access path, with each row's primary key.
pub fn scan<'a>(
    table: &'a Table,
    access: &'a Access,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'a>> {
    let Some((start, end, index)) = key_range(table, access, &env(&[], ctx, outer))? else {
        return Ok(Box::new(std::iter::empty()));
    };
    let end_bound = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    let entries = if ctx.validated {
        ctx.txn.scan_validated(Bound::Included(&start), end_bound)?
    } else {
        ctx.txn.scan(Bound::Included(&start), end_bound)?
    };
    Ok(match index {
        None => {
            let mut labels = crate::enums::Labels::default();
            Box::new(entries.map(move |e| {
                crate::budget::charge_scan()?;
                let (k, v) = e?;
                let row = crate::large::read_row_with(ctx.txn, table, &k, &v, &mut labels)?;
                Ok((k, row))
            }))
        }
        Some(_) => {
            let base = table.index_prefix(0);
            let mut labels = crate::enums::Labels::default();
            Box::new(entries.map(move |e| {
                crate::budget::charge_scan()?;
                let (_, pk) = e?;
                let mut k = base.clone();
                k.extend_from_slice(&pk);
                let v = ctx
                    .txn
                    .get(&k)?
                    .ok_or_else(|| Error::internal("index entry without its row"))?;
                let row = crate::large::read_row_with(ctx.txn, table, &k, &v, &mut labels)?;
                Ok((k, row))
            }))
        }
    })
}

/// Rows (with keys) of `table` matching `filter` — for UPDATE and DELETE.
pub fn matching_rows(
    table: &Table,
    access: &Access,
    filter: Option<&Expr>,
    ctx: &Ctx,
) -> Result<Vec<(Vec<u8>, Row)>> {
    let mut out = Vec::new();
    for r in scan(table, access, ctx, None)? {
        let (k, row) = r?;
        let keep = match filter {
            None => true,
            Some(f) => f.holds(&env(&row, ctx, None))?,
        };
        if keep {
            out.push((k, row));
        }
    }
    Ok(out)
}

/// The primary key of a row as stored (hidden row ids excluded).
pub fn pk_of(table: &Table, row: &[Value]) -> Vec<Value> {
    table.primary_key.iter().map(|&c| row[c].clone()).collect()
}

/// The key bytes after a table's primary index prefix.
pub fn pk_suffix(table: &Table, key: &[u8]) -> Vec<u8> {
    key[table.index_prefix(0).len()..].to_vec()
}

#[allow(clippy::too_many_arguments)]
fn join<'a>(
    left: &'a Plan,
    right: &'a Plan,
    kind: JoinType,
    on: Option<&'a Expr>,
    algo: &'a JoinAlgo,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    let right_rows = collect(right, ctx, outer)?;
    let right_width = right.width();
    let left_rows = build(left, ctx, outer)?;
    let matches = move |combined: &Row| -> Result<bool> {
        match on {
            None => Ok(true),
            Some(on) => on.holds(&env(combined, ctx, outer)),
        }
    };
    if kind == JoinType::Full {
        let widths = (left.width(), right_width);
        return full_join(widths, left_rows, right_rows, algo, &matches, ctx, outer);
    }
    match algo {
        JoinAlgo::NestedLoop => Ok(Box::new(left_rows.flat_map(move |l| -> Vec<Result<Row>> {
            let l = match l {
                Ok(l) => l,
                Err(e) => return vec![Err(e)],
            };
            let mut out = Vec::new();
            for r in &right_rows {
                let mut combined = l.clone();
                combined.extend_from_slice(r);
                match matches(&combined) {
                    Ok(true) => out.push(Ok(combined)),
                    Ok(false) => {}
                    Err(e) => return vec![Err(e)],
                }
            }
            if out.is_empty() && kind == JoinType::Left {
                let mut padded = l;
                padded.extend(std::iter::repeat_n(Value::Null, right_width));
                out.push(Ok(padded));
            }
            out
        }))),
        JoinAlgo::Hash {
            left_keys,
            right_keys,
        } => {
            let table = hash_table(&right_rows, right_keys, ctx, outer)?;
            Ok(Box::new(left_rows.flat_map(move |l| -> Vec<Result<Row>> {
                let l = match l {
                    Ok(l) => l,
                    Err(e) => return vec![Err(e)],
                };
                let key = match probe_key(&l, left_keys, ctx, outer) {
                    Ok(k) => k,
                    Err(e) => return vec![Err(e)],
                };
                let mut out = Vec::new();
                if let Some(key) = key {
                    for &i in table.get(&key).into_iter().flatten() {
                        let mut combined = l.clone();
                        combined.extend_from_slice(&right_rows[i]);
                        match matches(&combined) {
                            Ok(true) => out.push(Ok(combined)),
                            Ok(false) => {}
                            Err(e) => return vec![Err(e)],
                        }
                    }
                }
                if out.is_empty() && kind == JoinType::Left {
                    let mut padded = l;
                    padded.extend(std::iter::repeat_n(Value::Null, right_width));
                    out.push(Ok(padded));
                }
                out
            })))
        }
    }
}

/// The right rows of a hash join by key (rows with a NULL key left out:
/// NULL never equals anything).
fn hash_table(
    rows: &[Row],
    keys: &[Expr],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<HashMap<Vec<Value>, Vec<usize>>> {
    let mut table: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        let e = env(r, ctx, outer);
        let key = keys
            .iter()
            .map(|k| k.eval(&e))
            .collect::<Result<Vec<_>>>()?;
        if !key.iter().any(Value::is_null) {
            table.entry(key).or_default().push(i);
        }
    }
    Ok(table)
}

/// A left row's hash join key; `None` if it has a NULL.
fn probe_key(
    row: &[Value],
    keys: &[Expr],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Option<Vec<Value>>> {
    let e = env(row, ctx, outer);
    let key = keys
        .iter()
        .map(|k| k.eval(&e))
        .collect::<Result<Vec<_>>>()?;
    Ok((!key.iter().any(Value::is_null)).then_some(key))
}

/// A full outer join: the left join's rows, then every right row that
/// matched no left row, NULL-padded on the left.
fn full_join<'a>(
    (left_width, right_width): (usize, usize),
    left_rows: Rows<'a>,
    right_rows: Vec<Row>,
    algo: &JoinAlgo,
    matches: &dyn Fn(&Row) -> Result<bool>,
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Rows<'a>> {
    let table = match algo {
        JoinAlgo::Hash { right_keys, .. } => Some(hash_table(&right_rows, right_keys, ctx, outer)?),
        JoinAlgo::NestedLoop => None,
    };
    let all: Vec<usize> = (0..right_rows.len()).collect();
    let mut matched = vec![false; right_rows.len()];
    let mut out = Vec::new();
    for l in left_rows {
        let l = l?;
        let candidates: &[usize] = match (algo, &table) {
            (JoinAlgo::Hash { left_keys, .. }, Some(t)) => {
                match probe_key(&l, left_keys, ctx, outer)? {
                    Some(k) => t.get(&k).map_or(&[], Vec::as_slice),
                    None => &[],
                }
            }
            _ => &all,
        };
        let mut any = false;
        for &i in candidates {
            let mut combined = l.clone();
            combined.extend_from_slice(&right_rows[i]);
            if matches(&combined)? {
                matched[i] = true;
                any = true;
                out.push(combined);
            }
        }
        if !any {
            let mut padded = l;
            padded.extend(std::iter::repeat_n(Value::Null, right_width));
            out.push(padded);
        }
    }
    for (r, m) in right_rows.into_iter().zip(matched) {
        if !m {
            let mut padded = vec![Value::Null; left_width];
            padded.extend(r);
            out.push(padded);
        }
    }
    Ok(Box::new(out.into_iter().map(Ok)))
}

/// One aggregate's state in one group.
struct AggState {
    acc: Box<dyn Accumulator>,
    /// `DISTINCT`: the arguments seen.
    seen: Option<HashSet<Vec<Value>>>,
    /// `ORDER BY` in the call: (sort keys, arguments) until the end.
    buffer: Option<Vec<(Vec<Value>, Vec<Value>)>>,
}

/// Adds one row's arguments to an aggregate.
fn feed(agg: &Agg, st: &mut AggState, args: Vec<Value>) -> Result<()> {
    if !agg.func.keeps_nulls() && args.first().is_some_and(Value::is_null) {
        return Ok(());
    }
    if let Some(seen) = &mut st.seen
        && !seen.insert(args.clone())
    {
        return Ok(());
    }
    st.acc.add(&args)
}

fn aggregate(
    rows: Rows,
    group: &[Expr],
    aggs: &[Agg],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Vec<Row>> {
    let fresh = || -> Vec<AggState> {
        aggs.iter()
            .map(|a| AggState {
                acc: a.func.start(),
                seen: a.distinct.then(HashSet::new),
                buffer: (!a.order.is_empty()).then(Vec::new),
            })
            .collect()
    };
    let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
    for r in rows {
        let row = r?;
        let e = Env {
            row: &row,
            outer,
            ctx,
        };
        let key = group
            .iter()
            .map(|g| g.eval(&e))
            .collect::<Result<Vec<_>>>()?;
        let gi = match index.get(&key) {
            Some(&i) => i,
            None => {
                index.insert(key.clone(), groups.len());
                groups.push((key, fresh()));
                groups.len() - 1
            }
        };
        for (agg, st) in aggs.iter().zip(groups[gi].1.iter_mut()) {
            let args = agg
                .args
                .iter()
                .map(|a| a.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            if let Some(buffer) = &mut st.buffer {
                let keys = agg
                    .order
                    .iter()
                    .map(|k| k.expr.eval(&e))
                    .collect::<Result<Vec<_>>>()?;
                buffer.push((keys, args));
                continue;
            }
            feed(agg, st, args)?;
        }
    }
    if groups.is_empty() && group.is_empty() {
        groups.push((Vec::new(), fresh())); // aggregates over no rows
    }
    groups
        .into_iter()
        .map(|(mut key, states)| {
            for (agg, mut st) in aggs.iter().zip(states) {
                if let Some(mut buffer) = st.buffer.take() {
                    buffer.sort_by(|a, b| compare_keys(&agg.order, &a.0, &b.0));
                    for (_, args) in buffer {
                        feed(agg, &mut st, args)?;
                    }
                }
                key.push(st.acc.finish()?);
            }
            Ok(key)
        })
        .collect()
}

fn sort(rows: Rows, keys: &[SortKey], ctx: &Ctx, outer: Option<&Env>) -> Result<Vec<Row>> {
    let mut keyed = Vec::new();
    for r in rows {
        let row = r?;
        let e = Env {
            row: &row,
            outer,
            ctx,
        };
        let k = keys
            .iter()
            .map(|k| k.expr.eval(&e))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((k, row));
    }
    keyed.sort_by(|(a, _), (b, _)| compare_keys(keys, a, b));
    Ok(keyed.into_iter().map(|(_, r)| r).collect())
}

/// The order of two rows' sort key values.
fn compare_keys(keys: &[SortKey], a: &[Value], b: &[Value]) -> Ordering {
    for (i, k) in keys.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if k.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if k.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let o = x.sort_cmp(y);
                if k.desc { o.reverse() } else { o }
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// The encoded primary key of a row (hidden row ids passed in `rowid`).
pub fn row_key_of(table: &Table, row: &[Value], rowid: Option<i64>) -> Vec<u8> {
    match rowid {
        Some(id) => row_key(table, &[Value::Int(id)]),
        None => row_key(table, &pk_of(table, row)),
    }
}

/// The index entry key of `row` in `table.indexes[i]` (a plain index:
/// its columns' own values), and whether its index columns are all
/// non-NULL (only those take part in uniqueness).
pub fn index_entry(table: &Table, i: usize, row: &[Value], pk_bytes: &[u8]) -> (Vec<u8>, bool) {
    let vals: Vec<Value> = table.indexes[i]
        .columns
        .iter()
        .map(|&c| row[c].clone())
        .collect();
    index_key(table, i, vals, pk_bytes)
}

/// The index entry key for index values `vals` (the columns' values, or
/// their mappings in taught orderings) in `table.indexes[i]`.
pub fn index_key(table: &Table, i: usize, vals: Vec<Value>, pk_bytes: &[u8]) -> (Vec<u8>, bool) {
    let index = &table.indexes[i];
    let all_set = !vals.iter().any(Value::is_null);
    let mut k = table.index_prefix(index.id);
    k.extend_from_slice(&encode_key(&vals));
    // Unique entries are keyed by the index values alone, so two
    // transactions inserting the same value write the same key and the
    // second to commit conflicts; others carry the primary key too.
    if !(index.unique && all_set) {
        k.extend_from_slice(pk_bytes);
    }
    (k, index.unique && all_set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bounds_across_integer_widths() {
        // An integer of any width bounds a key of any width: the same bytes.
        for t in [DataType::SmallInt, DataType::Integer, DataType::BigInt] {
            assert_eq!(key_bound(Value::Int(5), t), Some(Value::Int(5)));
        }
        assert_eq!(
            key_bound(Value::Int(2), DataType::Float),
            Some(Value::Float(2.0))
        );
        assert_eq!(key_bound(Value::Text("5".into()), DataType::Integer), None);
    }
}
