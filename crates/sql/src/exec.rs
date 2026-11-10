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
use crate::encoding::encode_key_value;
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

/// Builds the iterator for `plan` (measured under `EXPLAIN ANALYZE`).
pub fn build<'a>(plan: &'a Plan, ctx: &'a Ctx<'a>, outer: Option<&'a Env<'a>>) -> Result<Rows<'a>> {
    match ctx.profile {
        Some(p) => p.measure(plan, || build_node(plan, ctx, outer)),
        None => build_node(plan, ctx, outer),
    }
}

fn build_node<'a>(
    plan: &'a Plan,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    // A batch at a time where it can be (PLAN.md §M14.20).
    match plan {
        Plan::Scan { .. } | Plan::Filter { .. } | Plan::Secure { .. } | Plan::Project { .. } => {
            if let Some(rows) = crate::vector::rows(plan, ctx, outer)? {
                return Ok(rows);
            }
        }
        Plan::Aggregate { input, group, aggs } => {
            if let Some(rows) = crate::vector::aggregate(input, plan, group, aggs, ctx, outer)? {
                return Ok(rows);
            }
        }
        _ => {}
    }
    Ok(match plan {
        Plan::Scan {
            table,
            access,
            filter,
            ..
        } => {
            // Only the columns the plan reads (`prune`).
            let wanted = ctx.needed.get(&(plan as *const Plan as usize));
            // The hash joins' key filters first, as a pipeline has them.
            let keys = crate::keyfilter::of_scan(ctx, plan);
            if let Some(rows) = method_rows(
                table,
                access,
                filter.as_ref(),
                keys.clone(),
                wanted,
                ctx,
                outer,
            )? {
                return Ok(rows);
            }
            // Where the index could not tell the rows: the filter's
            // B-tree range when it has one, else every row (and the
            // condition a method's index would have served).
            let (access, served) = unserved(access);
            let filter = match (served, filter) {
                (Some(c), Some(f)) => Some(Expr::Binary(
                    crate::ast::BinaryOp::And,
                    Box::new(c.clone()),
                    Box::new(f.clone()),
                )),
                (Some(c), None) => Some(c.clone()),
                (None, f) => f.clone(),
            };
            let rows = scan_some(table, access, ctx, outer, wanted.map(Vec::as_slice))?;
            let rows: Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>>> = if keys.is_empty() {
                Box::new(rows)
            } else {
                Box::new(rows.filter(move |r| {
                    r.as_ref()
                        .map_or(true, |(_, row)| crate::keyfilter::passes(&keys, row))
                }))
            };
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
            if let Some((shared, workers)) = rows_on_workers(&[predicate], ctx, outer) {
                let each = move |wctx: &Ctx, row: Row| match predicate.holds(&env(&row, wctx, None))
                {
                    Ok(true) => vec![Ok(row)],
                    Ok(false) => vec![],
                    Err(e) => vec![Err(e)],
                };
                return Ok(Box::new(OnWorkers::new(
                    rows,
                    ctx,
                    shared,
                    workers,
                    Box::new(each),
                )));
            }
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
            let all: Vec<&Expr> = exprs.iter().collect();
            if let Some((shared, workers)) = rows_on_workers(&all, ctx, outer) {
                let each = move |wctx: &Ctx, row: Row| {
                    let e = env(&row, wctx, None);
                    vec![exprs.iter().map(|x| x.eval(&e)).collect()]
                };
                return Ok(Box::new(OnWorkers::new(
                    rows,
                    ctx,
                    shared,
                    workers,
                    Box::new(each),
                )));
            }
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
        } => join(plan, left, right, *kind, on.as_ref(), algo, ctx, outer)?,
        Plan::Aggregate { input, group, aggs } => {
            aggregate(build(input, ctx, outer)?, group, aggs, ctx, outer, 0)?
        }
        Plan::Sort { input, keys } => {
            // Under a LIMIT: only the rows it takes kept (a top-N).
            let bound = ctx
                .bounds
                .borrow_mut()
                .remove(&(plan as *const Plan as usize));
            // A pipeline's rows: sorted in parallel, morsels merged.
            if let Some(rows) = crate::vector::sorted(input, keys, bound, ctx, outer)? {
                return Ok(rows);
            }
            // In memory while it fits in work_mem, else by runs merged.
            let keyed = build(input, ctx, outer)?.map(|r| {
                let row = r?;
                let e = env(&row, ctx, outer);
                let k = keys
                    .iter()
                    .map(|k| k.expr.eval(&e))
                    .collect::<Result<Vec<_>>>()?;
                Ok((k, row))
            });
            crate::spill::sort(
                keyed,
                keys.iter().map(|k| (k.desc, k.nulls_first)).collect(),
                bound,
            )?
        }
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
            // The rows it takes, for its input: a sort keeps only those (a
            // top-N), a scan reads on one thread (it stops early).
            if let Some(n) = limit
                && let Some(k) = n.checked_add(offset)
            {
                let mut bounds = ctx.bounds.borrow_mut();
                bounds.insert(&**input as *const Plan as usize, k);
                // Down a chain, each of its nodes is read row by row too.
                let mut node = &**input;
                while let Plan::Project { input, .. }
                | Plan::Filter { input, .. }
                | Plan::Secure { input, .. } = node
                {
                    node = input;
                    bounds.insert(node as *const Plan as usize, k);
                }
            }
            let rows = build(input, ctx, outer)?.skip(offset);
            match limit {
                Some(n) => Box::new(rows.take(n)),
                None => Box::new(rows),
            }
        }
        Plan::Distinct { input } => Box::new(Distinct::new(build(input, ctx, outer)?, 0)),
        Plan::Window { input, funcs } => {
            crate::window::run(build(input, ctx, outer)?, input.width(), funcs, ctx, outer)?
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
            let rows: Box<dyn Iterator<Item = Result<Row>>> = match func {
                TableFunc::Scalar => Box::new(std::iter::once(Ok(vec![vals[0].clone()]))),
                TableFunc::Call(f) => f.stream(&CallContext::new(ctx.txn, ctx.registry), &vals)?,
            };
            // Each row counts as read (generate_series counts its own).
            let charged = !matches!(func, TableFunc::Call(f) if f.name() == "generate_series");
            let ordinality = *ordinality;
            Box::new(rows.enumerate().map(move |(i, r)| {
                let mut row = r?;
                if charged {
                    crate::budget::charge_scan()?;
                }
                if ordinality {
                    row.push(Value::Int(i as i64 + 1));
                }
                Ok(row)
            }))
        }
        Plan::Cte { id, plan, .. } => {
            let cached = ctx.ctes.borrow().get(id).cloned();
            let rows = match cached {
                Some(rows) => rows,
                None => {
                    // Materialised once: in memory, or past work_mem in
                    // scratch space.
                    let rows = Rc::new(stash(build(plan, ctx, outer)?)?);
                    ctx.ctes.borrow_mut().insert(*id, Rc::clone(&rows));
                    rows
                }
            };
            rows.iter()
        }
        Plan::WorkTable { id, .. } => match ctx.work.borrow().get(id).cloned() {
            Some(rows) => rows.iter(),
            None => Box::new(std::iter::empty()),
        },
        Plan::Recursive {
            id,
            base,
            step,
            all,
        } => {
            let mut seen = HashSet::new();
            // The rows of a round not seen before (all of them for UNION
            // ALL), into the result and the next round.
            let mut out = crate::spill::Stash::new();
            let mut fresh = |rows: Rows, out: &mut crate::spill::Stash, counted: bool| {
                let mut round = crate::spill::Stash::new();
                for r in rows {
                    let r = r?;
                    if counted {
                        // Each row a round makes counts as read: an endless
                        // recursion runs into the statement's budget.
                        crate::budget::charge_scan()?;
                    }
                    if *all || seen.insert(r.clone()) {
                        out.push(r.clone())?;
                        round.push(r)?;
                    }
                }
                round.seal()
            };
            let mut round = fresh(build(base, ctx, outer)?, &mut out, false)?;
            while !round.is_empty() {
                // (The step reads the work table only outside subqueries
                // and cached CTEs, so no cache holds an earlier round.)
                ctx.work.borrow_mut().insert(*id, Rc::new(round));
                round = fresh(build(step, ctx, outer)?, &mut out, true)?;
            }
            ctx.work.borrow_mut().remove(id);
            out.seal()?.into_iter()
        }
        Plan::Append { inputs, guards, .. } => {
            // The partitions whose guards hold, each read when reached.
            let e = env(&[], ctx, outer);
            let mut keep = Vec::new();
            for (p, g) in inputs.iter().zip(guards) {
                if g.as_ref().map_or(Ok(true), |g| g.holds(&e))? {
                    keep.push(p);
                }
            }
            let mut keep = keep.into_iter();
            let mut cur: Option<Rows<'a>> = None;
            Box::new(std::iter::from_fn(move || {
                loop {
                    if let Some(rows) = &mut cur
                        && let Some(r) = rows.next()
                    {
                        return Some(r);
                    }
                    let p = keep.next()?;
                    match build(p, ctx, outer) {
                        Ok(rows) => cur = Some(rows),
                        Err(e) => {
                            cur = None;
                            return Some(Err(e));
                        }
                    }
                }
            }))
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
                Box::new(Distinct::new(Box::new(rows), 0))
            }
        }
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => set_op(
            *op,
            *all,
            build(left, ctx, outer)?,
            build(right, ctx, outer)?,
            0,
        )?,
    })
}

/// Rows without repeats: those seen kept in memory up to work_mem; past
/// it, rows not seen go to partitions (by value), each made distinct in
/// turn after the input.  A row seen before the memory filled is never
/// in a partition, so no row comes out twice.
struct Distinct<'a> {
    input: Option<Rows<'a>>,
    seen: HashSet<Row>,
    bytes: usize,
    limit: usize,
    level: u32,
    parts: Option<crate::spill::Partitions>,
    pending: std::collections::VecDeque<std::sync::Arc<crate::spill::Sealed>>,
    tail: Option<Rows<'static>>,
}

impl<'a> Distinct<'a> {
    fn new(input: Rows<'a>, level: u32) -> Self {
        Distinct {
            input: Some(input),
            seen: HashSet::new(),
            bytes: 0,
            limit: crate::budget::work_mem(),
            level,
            parts: None,
            pending: Default::default(),
            tail: None,
        }
    }

    fn step(&mut self) -> Result<Option<Row>> {
        loop {
            if let Some(input) = &mut self.input {
                let Some(row) = input.next().transpose()? else {
                    self.input = None;
                    self.seen = HashSet::new();
                    if let Some(p) = self.parts.take() {
                        self.pending = p.seal()?.into();
                    }
                    continue;
                };
                if self.seen.contains(&row) {
                    continue;
                }
                let size = crate::spill::row_bytes(&row);
                if self.parts.is_none()
                    && (self.bytes + size <= self.limit || self.level >= crate::spill::MAX_LEVEL)
                {
                    self.bytes += size;
                    self.seen.insert(row.clone());
                    return Ok(Some(row));
                }
                self.parts
                    .get_or_insert_with(|| crate::spill::Partitions::new(self.level))
                    .push(&row, &row)?;
            } else if let Some(tail) = &mut self.tail {
                match tail.next().transpose()? {
                    Some(row) => return Ok(Some(row)),
                    None => self.tail = None,
                }
            } else if let Some(run) = self.pending.pop_front() {
                self.tail = Some(Box::new(Distinct::new(
                    Box::new(run.iter()),
                    self.level + 1,
                )));
            } else {
                return Ok(None);
            }
        }
    }
}

impl Iterator for Distinct<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        self.step().transpose()
    }
}

/// `INTERSECT` / `EXCEPT` [`ALL`]: how often each right row occurs, and
/// left rows matched against it (multiset semantics for ALL, as in
/// PostgreSQL).  Past work_mem, both sides are partitioned by value and
/// each pair of partitions taken in turn.
fn set_op<'a>(
    op: SetOp,
    all: bool,
    left: Rows<'a>,
    right: Rows<'a>,
    level: u32,
) -> Result<Rows<'a>> {
    let limit = crate::budget::work_mem();
    let mut counts: HashMap<Row, usize> = HashMap::new();
    let mut bytes = 0;
    let mut right_parts: Option<crate::spill::Partitions> = None;
    for r in right {
        let row = r?;
        if let Some(p) = &mut right_parts {
            p.push(&row, &row)?;
            continue;
        }
        if let Some(n) = counts.get_mut(&row) {
            *n += 1;
            continue;
        }
        let size = crate::spill::row_bytes(&row);
        if bytes + size > limit && level < crate::spill::MAX_LEVEL {
            let mut p = crate::spill::Partitions::new(level);
            for (r, n) in counts.drain() {
                for _ in 0..n {
                    p.push(&r, &r)?;
                }
            }
            p.push(&row, &row)?;
            right_parts = Some(p);
            continue;
        }
        bytes += size;
        counts.insert(row, 1);
    }
    let Some(right_parts) = right_parts else {
        let kept = left.filter(move |r| {
            let Ok(row) = r else { return true };
            match (op, counts.get_mut(row)) {
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
            }
        });
        // Without ALL each distinct row appears at most once, and EXCEPT
        // drops a row the right side has at all.
        return Ok(if all {
            Box::new(kept)
        } else {
            Box::new(Distinct::new(Box::new(kept), level))
        });
    };
    let mut left_parts = crate::spill::Partitions::new(level);
    for r in left {
        let row = r?;
        left_parts.push(&row, &row)?;
    }
    let pairs: Vec<_> = left_parts
        .seal_all()?
        .into_iter()
        .zip(right_parts.seal_all()?)
        .collect();
    let side = |run: Option<std::sync::Arc<crate::spill::Sealed>>| -> Rows<'static> {
        match run {
            Some(r) => Box::new(r.iter()),
            None => Box::new(std::iter::empty()),
        }
    };
    Ok(Box::new(pairs.into_iter().flat_map(
        move |(l, r)| match set_op(op, all, side(l), side(r), level + 1) {
            Ok(rows) => rows,
            Err(e) => Box::new(std::iter::once(Err(e))),
        },
    )))
}

/// `generate_series(start, stop [, step])`: numbers, or timestamps with an
/// interval step.  NULL arguments give no rows.  Its rows, materialised
/// (at most 100 million).
pub(crate) fn generate_series(vals: &[Value]) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    for r in series(vals)? {
        if out.len() >= 100_000_000 {
            return Err(Error::out_of_range("generate_series: too many rows"));
        }
        out.push(r?);
    }
    Ok(out)
}

/// `generate_series`'s rows as they are read (the built-in's stream).
pub(crate) fn series(vals: &[Value]) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    use crate::ast::BinaryOp;
    use crate::expr::binary;
    if vals.iter().any(Value::is_null) {
        return Ok(Box::new(std::iter::empty()));
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
    let mut cur = Some(start);
    Ok(Box::new(std::iter::from_fn(move || {
        let c = cur.take()?;
        let o = match c.sql_cmp(&stop) {
            Ok(o) => o.expect("not null"),
            Err(e) => return Some(Err(e)),
        };
        if (up && o == Ordering::Greater) || (!up && o == Ordering::Less) {
            return None;
        }
        // Each row made counts as read (and the statement's time runs).
        if let Err(e) = crate::budget::charge_scan() {
            return Some(Err(e));
        }
        match binary(BinaryOp::Add, c.clone(), step.clone()) {
            Ok(v) => cur = Some(v),
            // Past the end of the type: this was the last.
            Err(e) if e.code == "22003" || e.code == "22008" => {}
            Err(e) => return Some(Err(e)),
        }
        Some(Ok(vec![c]))
    })))
}

/// `rows` materialised: in memory, or past work_mem in scratch space.
fn stash(rows: Rows) -> Result<crate::spill::Stored> {
    let mut s = crate::spill::Stash::new();
    for r in rows {
        s.push(r?)?;
    }
    s.seal()
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
pub(crate) fn key_range(
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

/// The access path a scan through an index method reads where the index
/// cannot tell its rows, and the condition it served, then checked on
/// each: every row, or the B-tree range or the other method's condition
/// a ranking was within.
fn unserved(access: &Access) -> (&Access, Option<&Expr>) {
    match access {
        Access::Method {
            served: Some(s), ..
        } => (&Access::Full, (!s.above).then_some(&s.cond)),
        Access::Method { rank: Some(r), .. } => match r.within.as_deref() {
            Some(w) => unserved(w),
            None => (&Access::Full, None),
        },
        Access::Method { .. } => (&Access::Full, None),
        a => (a, None),
    }
}

/// The rows of a scan through [`Access::Method`] — those its index finds
/// for the condition it serves, or first by its rank — that pass the rest
/// of the filter and the key filters; `None` (the caller reads every row)
/// for any other access path, or where the index cannot tell them for
/// these values.
fn method_rows<'a>(
    table: &'a Table,
    access: &Access,
    filter: Option<&'a Expr>,
    keys: Vec<(usize, std::sync::Arc<crate::keyfilter::KeyFilter>)>,
    wanted: Option<&'a Vec<bool>>,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    let Access::Method {
        index,
        served,
        rank,
    } = access
    else {
        return Ok(None);
    };
    let Some(m) = crate::index_method::of(&table.indexes[*index])? else {
        return Ok(None);
    };
    let e = env(&[], ctx, outer);
    let served = match served {
        Some(s) => Some((
            s.function.as_str(),
            s.args
                .iter()
                .map(|a| a.eval(&e))
                .collect::<Result<Vec<_>>>()?,
        )),
        None => None,
    };
    if let Some(r) = rank {
        let ranked = ranked_rows(
            table,
            *index,
            &*m,
            served.as_ref().map(|(f, v)| (*f, v.as_slice())),
            r,
            filter,
            &keys,
            wanted,
            ctx,
            outer,
        )?;
        if let Some(rows) = ranked {
            return Ok(Some(Box::new(rows.into_iter().map(|(_, row)| Ok(row)))));
        }
        // Not ranked: the rows of the other method's condition it was
        // within, where that tells them.
        if served.is_none() {
            return match r.within.as_deref() {
                Some(w @ Access::Method { .. }) => {
                    method_rows(table, w, filter, keys, wanted, ctx, outer)
                }
                _ => Ok(None),
            };
        }
    }
    let Some((function, values)) = served else {
        return Ok(None);
    };
    let Some(pks) = m.rows_where(ctx.txn, table, *index, function, &values)? else {
        return Ok(None);
    };
    crate::budget::charge_scans(pks.len() as u64)?;
    // No column read, nothing else to check (`count(*)`): the index is
    // exact, so its rows need not be read.
    if wanted.is_some_and(|w| !w.contains(&true)) && filter.is_none() && keys.is_empty() {
        let width = table.columns.len();
        return Ok(Some(Box::new(
            pks.into_iter().map(move |_| Ok(vec![Value::Null; width])),
        )));
    }
    // The rows in key order, a block at a time: a merge with the table's
    // scan ([`rows_at`]).
    let prefix = table.index_prefix(0);
    let (mut at, mut block, mut density) = (0usize, FOUND_BLOCK, None);
    let blocks = std::iter::from_fn(move || -> Option<Result<Vec<Row>>> {
        if at >= pks.len() {
            return None;
        }
        let end = (at + block).min(pks.len());
        let found: Vec<Vec<u8>> = pks[at..end]
            .iter()
            .map(|pk| [prefix.as_slice(), pk].concat())
            .collect();
        at = end;
        block = (block * if block < FOUND_PART { 2 } else { 4 }).min(FOUND_BLOCK_MOST);
        Some(
            rows_at(
                ctx.txn,
                table,
                &found,
                wanted.map(Vec::as_slice),
                &mut density,
            )
            .and_then(|rows| {
                if rows.len() < found.len() {
                    return Err(Error::internal(
                        "an index method found a row its table does not have",
                    ));
                }
                Ok(rows.into_iter().map(|(_, row)| row).collect())
            }),
        )
    });
    let rows = blocks.flat_map(|b| match b {
        Ok(rows) => rows.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(e) => vec![Err(e)],
    });
    Ok(Some(Box::new(rows.filter_map(move |row| {
        let row = match row {
            Ok(row) => row,
            Err(e) => return Some(Err(e)),
        };
        if !crate::keyfilter::passes(&keys, &row) {
            return None;
        }
        match filter.map(|f| f.holds(&env(&row, ctx, outer))) {
            None | Some(Ok(true)) => Some(Ok(row)),
            Some(Ok(false)) => None,
            Some(Err(e)) => Some(Err(e)),
        }
    }))))
}

/// The found rows a scan through an index method reads first; then each
/// block twice the last while one worker reads it ([`FOUND_PART`]), four
/// times past that, up to [`FOUND_BLOCK_MOST`] — a limit above stops it
/// early, having read at most twice its rows while they are few.
const FOUND_BLOCK: usize = 8;
const FOUND_BLOCK_MOST: usize = 16_384;

/// The rows a method's rank gives ([`crate::index_method::IndexMethod::rank`]),
/// each passing the key filters, the rank's `within` condition (unless
/// the method ranked among its rows), the filter and the rank's checks,
/// in that order; `None` where the index cannot tell them.
#[allow(clippy::too_many_arguments)]
fn ranked_rows(
    table: &Table,
    index: usize,
    m: &dyn crate::index_method::IndexMethod,
    served: Option<(&str, &[Value])>,
    r: &crate::plan::MethodRank,
    filter: Option<&Expr>,
    keys: &[(usize, std::sync::Arc<crate::keyfilter::KeyFilter>)],
    wanted: Option<&Vec<bool>>,
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
    let e = env(&[], ctx, outer);
    let Value::Int(k) = r.count.eval(&e)? else {
        return Ok(None);
    };
    let Ok(k) = usize::try_from(k) else {
        return Ok(None);
    };
    let rank = crate::index_method::Rank {
        function: r.function.clone(),
        args: r.args.iter().map(|a| a.eval(&e)).collect::<Result<_>>()?,
        desc: r.desc,
        k,
    };
    // An index of more columns: their values (`col = NULL` matches
    // nothing — the method's to say; a value its column's key cannot
    // hold, the scan).
    let mut part = Vec::with_capacity(r.partition.len());
    for (v, &c) in r.partition.iter().zip(&table.indexes[index].columns[1..]) {
        let Some(v) = key_bound(v.eval(&e)?, table.columns[c].ty) else {
            return Ok(None);
        };
        part.push(v);
    }
    let within = r.within.as_deref();
    let chose = match within {
        Some(Access::Method {
            served: Some(s), ..
        }) => Some(&s.cond),
        _ => None,
    };
    let mut keep = |row: &Row, among: bool| -> Result<bool> {
        if !crate::keyfilter::passes(keys, row) {
            return Ok(false);
        }
        let e = env(row, ctx, outer);
        let chose = chose.filter(|_| !among);
        for c in chose.into_iter().chain(filter).chain(&r.checks) {
            if !c.holds(&e)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    let free = keys.is_empty() && chose.is_none() && filter.is_none() && r.checks.is_empty();
    let mut get = |most: usize| match within {
        Some(w) => among(table, w, most, ctx, outer),
        None => Ok(None),
    };
    let mut ask = crate::index_method::RankAsk {
        rank: &rank,
        served,
        share: r.share,
        part: &part,
        among: within
            .is_some()
            .then_some(&mut get as crate::index_method::Among),
    };
    m.rank(
        ctx.txn,
        table,
        index,
        &mut ask,
        wanted.map(Vec::as_slice),
        (!free).then_some(&mut keep as crate::index_method::Keep),
    )
}

/// The rows (primary keys) a ranking is within — a filter's B-tree key
/// range, or another method's condition (`w`) —, for a search among them:
/// `None` where they are more than `most`.
fn among(
    table: &Table,
    w: &Access,
    most: usize,
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Option<crate::index_method::Pks>> {
    let mut pks = crate::index_method::Pks::default();
    if let Access::Method {
        index,
        served: Some(s),
        ..
    } = w
    {
        let Some(m) = crate::index_method::of(&table.indexes[*index])? else {
            return Ok(None);
        };
        let e = env(&[], ctx, outer);
        let values = s
            .args
            .iter()
            .map(|a| a.eval(&e))
            .collect::<Result<Vec<_>>>()?;
        let Some(found) = m.rows_where(ctx.txn, table, *index, &s.function, &values)? else {
            return Ok(None);
        };
        crate::budget::charge_scans(found.len() as u64)?;
        if found.len() > most {
            return Ok(None);
        }
        for pk in &found {
            pks.push(pk);
        }
        return Ok(Some(pks));
    }
    let Some((start, end, Some(_))) = key_range(table, w, &env(&[], ctx, outer))? else {
        return Ok(Some(pks));
    };
    let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    let mut entries = Entries::new(ctx, Bound::Included(&start), end)?;
    while let Some((_, pk)) = entries.next()? {
        pks.push(pk);
        if pks.len() > most {
            crate::budget::charge_scans(pks.len() as u64)?;
            return Ok(None);
        }
    }
    crate::budget::charge_scans(pks.len() as u64)?;
    Ok(Some(pks))
}

/// A table's rows through an access path, with each row's primary key.
pub fn scan<'a>(
    table: &'a Table,
    access: &'a Access,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'a>> {
    scan_some(table, access, ctx, outer, None)
}

/// [`scan`], decoding only the columns `wanted` marks (the others NULL).
pub fn scan_some<'a>(
    table: &'a Table,
    access: &'a Access,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
    wanted: Option<&'a [bool]>,
) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'a>> {
    let Some((start, end, index)) = key_range(table, access, &env(&[], ctx, outer))? else {
        return Ok(Box::new(std::iter::empty()));
    };
    let storage = crate::storage::of(table)?;
    if index.is_none() {
        let rows = storage.rows(crate::storage::Scan {
            txn: ctx.txn,
            table,
            start,
            end,
            wanted,
            validated: ctx.validated,
            bounds: Vec::new(),
            dicts: Vec::new(),
        })?;
        return Ok(Box::new(rows.map(|r| {
            crate::budget::charge_scan()?;
            r
        })));
    }
    // A secondary index: its entries hold the rows' primary key suffixes.
    let end_bound = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    let entries = if ctx.validated {
        ctx.txn.scan_validated(Bound::Included(&start), end_bound)?
    } else {
        ctx.txn.scan(Bound::Included(&start), end_bound)?
    };
    let base = table.index_prefix(0);
    let mut labels = crate::enums::Labels::default();
    Ok(Box::new(entries.map(move |e| {
        crate::budget::charge_scan()?;
        let (_, pk) = e?;
        let mut k = base.clone();
        k.extend_from_slice(&pk);
        let row = storage
            .get(ctx.txn, table, &k, wanted, &mut labels)?
            .ok_or_else(|| Error::internal("index entry without its row"))?;
        Ok((k, row))
    })))
}

/// A key range's entries read a chunk at a time, each borrowed from the
/// chunk ([`nucleus_kv::KvTxn::scan_chunks`]).
pub struct Entries<'a> {
    chunks: Box<dyn nucleus_kv::KvChunks + 'a>,
    chunk: nucleus_kv::Chunk,
    pos: usize,
    done: bool,
}

impl<'a> Entries<'a> {
    /// The entries in `(start, end)`, read as the statement reads
    /// (validated when it must be).
    pub fn new(ctx: &'a Ctx<'a>, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Self> {
        Self::over(ctx.txn, ctx.validated, start, end)
    }

    /// The entries in `(start, end)` of `txn` (`validated`: reads checked
    /// at commit).
    pub fn over(
        txn: &'a dyn nucleus_kv::KvTxn,
        validated: bool,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Self> {
        let chunks = if validated {
            txn.scan_chunks_validated(start, end)?
        } else {
            txn.scan_chunks(start, end)?
        };
        Ok(Self {
            chunks,
            chunk: nucleus_kv::Chunk::new(),
            pos: 0,
            done: false,
        })
    }

    /// The next entry.
    #[allow(clippy::should_implement_trait)] // it lends from the chunk
    #[inline]
    pub fn next(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        if self.pos == self.chunk.len() {
            if self.done {
                return Ok(None);
            }
            self.pos = 0;
            self.chunk.clear();
            if !self.chunks.next_chunk(&mut self.chunk)? {
                self.done = true;
                return Ok(None);
            }
        }
        self.pos += 1;
        let i = self.pos - 1;
        Ok(Some((self.chunk.key(i), self.chunk.value(i))))
    }
}

/// The entries under `keys` (sorted, each once) of `txn`, below `end`,
/// read by a merge with the store's scan (PLAN.md §M14.21 T5b): `f` gets
/// each key's position, the key and its value (keys with no entry are
/// left out).  Where the keys lie far apart each is looked up — a
/// descent; where they lie close the scan reads on through the entries
/// between, a chunk at a time, as a scan does, passing the unwanted by
/// their keys — PostgreSQL's bitmap heap scan, Lucene's `advance`.  Which
/// is cheaper depends on the share of the entries passed that are
/// wanted: a lookup costs about what reading [`SEEK_ENTRIES`] entries
/// does, so reading on pays above one in that many.  The share is
/// measured as it goes — each chunk read on, a probe of [`PROBE`] entries
/// where it is not yet known and every [`PROBE_EVERY`]th lookup — and
/// kept in `density` for the caller's next keys (a statement's blocks of
/// found rows share it), so where it is known to be low no scan is
/// opened.
pub fn entries_at(
    txn: &dyn nucleus_kv::KvTxn,
    validated: bool,
    keys: &[Vec<u8>],
    end: Bound<&[u8]>,
    density: &mut Option<f64>,
    f: &mut dyn FnMut(usize, &[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let Some(last) = keys.last() else {
        return Ok(());
    };
    // The scan ends at the last key: past it, reading on would pass
    // entries none wanted.
    let end = match end {
        Bound::Excluded(e) if e <= last.as_slice() => end,
        Bound::Included(e) if e < last.as_slice() => end,
        _ => Bound::Included(last.as_slice()),
    };
    let dense_at = |d: f64| d * SEEK_ENTRIES as f64 > 1.0;
    let mut chunks: Option<Box<dyn nucleus_kv::KvChunks + '_>> = None;
    let mut chunk = nucleus_kv::Chunk::new();
    let mut i = 0;
    // The keys wanted from the next one on: the scan passes over the
    // others by their keys, their values not read.
    let next = std::cell::Cell::new(0usize);
    let want = |k: &[u8]| {
        let mut j = next.get();
        while j < keys.len() && keys[j].as_slice() < k {
            j += 1;
        }
        next.set(j);
        j < keys.len() && keys[j] == k
    };
    // Reading on, or looking each key up, a probe where the share is not
    // known and every [`PROBE_EVERY`]th lookup; the scan, opened at the
    // first key that reads it, is behind after lookups and sought on (one
    // that cannot seek reads on).
    let mut dense = density.is_some_and(dense_at);
    let (mut probe_due, mut lookups, mut behind, mut can_seek) =
        (density.is_none(), 0usize, false, true);
    while i < keys.len() {
        if !dense && !probe_due {
            let k = keys[i].as_slice();
            let v = if validated {
                txn.get_validated(k)?
            } else {
                txn.get(k)?
            };
            if let Some(v) = v {
                f(i, k, &v)?;
            }
            i += 1;
            lookups += 1;
            probe_due = lookups % PROBE_EVERY == 0;
            behind = true;
            continue;
        }
        let fresh = chunks.is_none();
        let c = match chunks.as_mut() {
            Some(c) => c,
            None => {
                let from = Bound::Included(keys[i].as_slice());
                behind = false;
                chunks.insert(if validated {
                    txn.scan_chunks_validated(from, end)?
                } else {
                    txn.scan_chunks(from, end)?
                })
            }
        };
        let behind_was = behind || fresh;
        if behind {
            behind = false;
            can_seek = c.seek(&keys[i])?;
            dense |= !can_seek;
        }
        // Reading on, and a probe, pass over at most what the lookups of
        // the keys they may find would cost (a scan that cannot seek,
        // anything).
        let n = if dense {
            nucleus_kv::CHUNK_ENTRIES
        } else {
            PROBE
        }
        .min(keys.len() - i);
        let most = if !can_seek {
            usize::MAX
        } else if dense {
            n * SEEK_ENTRIES
        } else {
            2 * SEEK_ENTRIES
        };
        // The scan is at a key wanted (opened or sought to it), or reads
        // on: the entry it starts at is not a sample of the share.
        let at_key = !dense || behind_was;
        probe_due = false;
        next.set(i);
        let mut skipped = 0;
        if !c.next_chunk_where(&mut chunk, n, Some(&want), &mut skipped, most)? {
            return Ok(());
        }
        // The keys this chunk holds, each found by a binary search over
        // the rest of it (a scan that cannot pass over others gives them
        // too).
        let mut pos = 0;
        let len = chunk.len();
        while len > 0 && i < keys.len() && keys[i].as_slice() <= chunk.key(len - 1) {
            let k = keys[i].as_slice();
            let (mut lo, mut hi) = (pos, len);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if chunk.key(mid) < k {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            pos = lo;
            if pos < len && chunk.key(pos) == k {
                f(i, k, chunk.value(pos))?;
                pos += 1;
            }
            i += 1;
        }
        let first = usize::from(at_key && len > 0);
        if can_seek && len + skipped > first {
            // The share wanted of the entries passed: reading on pays
            // above one in what a lookup costs.
            let d = (len - first) as f64 / (len + skipped - first) as f64;
            *density = Some(d);
            dense = dense_at(d);
        }
    }
    Ok(())
}

/// The rows of `t` under `keys` (sorted, each once), as `txn` sees them,
/// the columns `wanted` marks decoded, each with its key's position —
/// those there are ([`crate::storage::TableStorage::rows_at`]: a merge
/// with the table's scan).  Many are read on the statement's workers, the
/// keys cut into as many runs, each merged with a scan of its own over
/// the transaction's shared reads — PostgreSQL's parallel bitmap heap
/// scan; the rows come back in key order.
pub fn rows_at(
    txn: &dyn nucleus_kv::KvTxn,
    t: &Table,
    keys: &[Vec<u8>],
    wanted: Option<&[bool]>,
    density: &mut Option<f64>,
) -> Result<Vec<(usize, Row)>> {
    let storage = crate::storage::of(t)?;
    let workers = crate::budget::workers().min(keys.len() / FOUND_PART);
    let shared = match txn.shared() {
        Some(s) if workers > 1 => s,
        _ => {
            let mut out = Vec::with_capacity(keys.len());
            storage.rows_at(txn, t, keys, wanted, density, &mut |i, row| {
                out.push((i, row));
                Ok(())
            })?;
            return Ok(out);
        }
    };
    let part = keys.len().div_ceil(workers);
    // Each worker from the share known so far, measuring its own.
    let start = *density;
    let ambient = crate::parallel::Ambient::capture();
    let instance = txn.instance();
    let parts: Vec<Result<Vec<(usize, Row)>>> = std::thread::scope(|sc| {
        let handles: Vec<_> = keys
            .chunks(part)
            .enumerate()
            .map(|(m, run)| {
                let (storage, ambient) = (&storage, &ambient);
                sc.spawn(move || {
                    let (rows, scanned) = ambient.run(|| {
                        let txn = crate::parallel::SharedTxn(shared, instance);
                        let mut rows = Vec::with_capacity(run.len());
                        let mut density = start;
                        storage.rows_at(&txn, t, run, wanted, &mut density, &mut |i, row| {
                            rows.push((i + m * part, row));
                            Ok(())
                        })?;
                        Ok(rows)
                    });
                    crate::budget::add_scanned(scanned);
                    rows
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(Error::internal("a worker panicked")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(keys.len());
    for p in parts {
        out.extend(p?);
    }
    Ok(out)
}

/// The fewest found rows a worker reads ([`rows_at`]): a thread costs
/// tens of µs to start, a row about one to read.
const FOUND_PART: usize = 256;

/// What a lookup costs, in entries read on past (measured: PLAN.md
/// §M14.21 T5b).
const SEEK_ENTRIES: usize = 16;
/// The entries a probe takes at most.
const PROBE: usize = 16;
/// Lookups between probes.
const PROBE_EVERY: usize = 1024;

/// Rows (with keys) of `table` matching `filter`, as they are read.
pub fn matching<'a>(
    table: &'a Table,
    access: &'a Access,
    filter: Option<&'a Expr>,
    ctx: &'a Ctx<'a>,
) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'a>> {
    Ok(Box::new(scan(table, access, ctx, None)?.filter_map(
        move |r| {
            let (k, row) = match r {
                Ok(x) => x,
                Err(e) => return Some(Err(e)),
            };
            match filter.map_or(Ok(true), |f| f.holds(&env(&row, ctx, None))) {
                Ok(true) => Some(Ok((k, row))),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            }
        },
    )))
}

/// A row with its key, as a stash keeps it: the key first, as `bytea`.
pub fn keyed(key: Vec<u8>, row: Row) -> Row {
    let mut out = Vec::with_capacity(row.len() + 1);
    out.push(Value::Bytea(key.into()));
    out.extend(row);
    out
}

/// The key and row of a [`keyed`] row.
pub fn unkeyed(mut row: Row) -> Result<(Vec<u8>, Row)> {
    let rest = row.split_off(1);
    match row.pop() {
        Some(Value::Bytea(k)) => Ok((k.to_vec(), rest)),
        _ => Err(Error::internal("a stashed row without its key")),
    }
}

/// Every keyed row of `table` matching `filter`, stashed (past work_mem in
/// scratch space): what a write that reads its own table collects first.
pub fn matching_stash(
    table: &Table,
    access: &Access,
    filter: Option<&Expr>,
    ctx: &Ctx,
) -> Result<crate::spill::Stored> {
    if let Some(s) = matching_stash_vectorized(table, access, filter, ctx)? {
        return Ok(s);
    }
    let mut s = crate::spill::Stash::new();
    for r in matching(table, access, filter, ctx)? {
        let (k, row) = r?;
        s.push(keyed(k, row))?;
    }
    s.seal()
}

/// [`matching_stash`] through the vectorized executor — the scan and the
/// filter a batch at a time, in parallel when they can be — for a table
/// whose rows are keyed by their primary key (each row's key is computed
/// from its values, so the scan need not carry it); `None` when the scan
/// cannot run so.  The rows, their order and the errors are a single
/// pass's ([`crate::vector::rows`]).
fn matching_stash_vectorized(
    table: &Table,
    access: &Access,
    filter: Option<&Expr>,
    ctx: &Ctx,
) -> Result<Option<crate::spill::Stored>> {
    if table.primary_key.is_empty() {
        return Ok(None);
    }
    let plan = Plan::Scan {
        table: table.clone(),
        access: access.clone(),
        filter: filter.cloned(),
        estimate: None,
    };
    let Some(rows) = crate::vector::rows(&plan, ctx, None)? else {
        return Ok(None);
    };
    let mut s = crate::spill::Stash::new();
    for r in rows {
        let row = r?;
        s.push(keyed(row_key_of(table, &row, None), row))?;
    }
    Ok(Some(s.seal()?))
}

/// Rows (with keys) of `table` matching `filter`.
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
pub fn pk_suffix<'k>(_table: &Table, key: &'k [u8]) -> &'k [u8] {
    &key[crate::catalog::INDEX_PREFIX_LEN..]
}

/// What a join needs besides its inputs.
#[derive(Clone, Copy)]
struct JoinSpec<'a> {
    kind: JoinType,
    on: Option<&'a Expr>,
    algo: &'a JoinAlgo,
    left_width: usize,
    right_width: usize,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
}

impl<'a> JoinSpec<'a> {
    fn matches(&self, combined: &Row) -> Result<bool> {
        match self.on {
            None => Ok(true),
            Some(on) => on.holds(&env(combined, self.ctx, self.outer)),
        }
    }

    /// A left row with NULLs for the right's columns.
    fn left_padded(&self, mut l: Row) -> Row {
        l.extend(std::iter::repeat_n(Value::Null, self.right_width));
        l
    }

    /// A right row with NULLs for the left's columns.
    fn right_padded(&self, r: &[Value]) -> Row {
        let mut padded = vec![Value::Null; self.left_width];
        padded.extend_from_slice(r);
        padded
    }

    /// What an unmatched left row gives: padded (LEFT, FULL), itself (an
    /// anti join), or nothing.
    fn unmatched(&self, l: Row) -> Option<Row> {
        match self.kind {
            JoinType::Left | JoinType::Full => Some(self.left_padded(l)),
            JoinType::Anti => Some(l),
            JoinType::Inner | JoinType::Semi => None,
        }
    }

    /// Whether a left row comes out once, by itself, when matched or not
    /// (semi and anti joins).
    fn existence(&self) -> bool {
        matches!(self.kind, JoinType::Semi | JoinType::Anti)
    }

    /// The hash join keys, if any.
    fn keys(&self) -> Option<(&'a [Expr], &'a [Expr])> {
        match self.algo {
            JoinAlgo::Hash {
                left_keys,
                right_keys,
                ..
            } => Some((left_keys, right_keys)),
            JoinAlgo::NestedLoop | JoinAlgo::Lookup => None,
        }
    }
}

/// Joins `left` with `right`: the right input is stashed (past work_mem in
/// scratch space), then joined in memory, by a grace hash join (both sides
/// partitioned by key, pairs joined in turn), or by blocks of left rows
/// read against it (a block nested loop).
#[allow(clippy::too_many_arguments)]
fn join<'a>(
    node: &'a Plan,
    left: &'a Plan,
    right: &'a Plan,
    kind: JoinType,
    on: Option<&'a Expr>,
    algo: &'a JoinAlgo,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    let spec = JoinSpec {
        kind,
        on,
        algo,
        left_width: left.width(),
        right_width: right.width(),
        ctx,
        outer,
    };
    if *algo == JoinAlgo::Lookup {
        return lookup_join(build(left, ctx, outer)?, right, spec);
    }
    // A probe side's run that did not become a pipeline made this join's
    // build side already (`crate::vector::prepare`): it is taken.
    let made = match algo {
        JoinAlgo::Hash {
            build_left: false, ..
        } => crate::vector::take_built(ctx, node),
        _ => None,
    };
    // An adaptive join: its right side made as the hash join makes it,
    // then looked at — few rows look the left side up instead.
    let made = match algo {
        JoinAlgo::Hash {
            left_keys,
            right_keys,
            adaptive: Some(a),
            ..
        } => {
            use crate::vector::{Built, Gathered, JoinTable};
            let (built, fresh) = match made {
                Some(b) => (b, false),
                None => {
                    let mut table = None;
                    if crate::vector::probe_side(left) && crate::vector::probes(right) {
                        table = JoinTable::parallel(right, right_keys, ctx, outer)?;
                    }
                    let built = match table {
                        Some(t) => Built::Table(std::sync::Arc::new(t)),
                        None if crate::vector::probe_side(left) => {
                            match JoinTable::gather(
                                build(right, ctx, outer)?,
                                right.width(),
                                right_keys,
                                ctx,
                                outer,
                            )? {
                                Gathered::Table(t) => Built::Table(std::sync::Arc::new(t)),
                                Gathered::Stored(rows) => Built::Stored(rows),
                            }
                        }
                        None => Built::Stored(stash(build(right, ctx, outer)?)?),
                    };
                    (built, true)
                }
            };
            let built = match built {
                Built::Table(t) if t.len() <= a.below => {
                    return adaptive_lookups(t.rows_in_order(), a, spec);
                }
                Built::Stored(crate::spill::Stored::Memory(rows)) if rows.len() <= a.below => {
                    return adaptive_lookups(rows, a, spec);
                }
                b => b,
            };
            if let Built::Table(table) = &built
                && fresh
            {
                crate::keyfilter::register(ctx, left, right, kind, left_keys, &|j| {
                    table.key_ints(j)
                });
            }
            Some(built)
        }
        _ => made,
    };
    let made = match made {
        Some(crate::vector::Built::Table(table)) => {
            let JoinAlgo::Hash {
                left_keys,
                right_keys,
                ..
            } = algo
            else {
                unreachable!("a hash join's table")
            };
            return probe_or_join(
                crate::vector::hash_join_table(
                    left,
                    table,
                    left_keys,
                    right_keys,
                    kind,
                    on,
                    right.width(),
                    ctx,
                    outer,
                )?,
                left,
                spec,
            );
        }
        Some(crate::vector::Built::Stored(rows)) => Some(rows),
        None => None,
    };
    // A semi or anti join hashed on its left side (the smaller), when the
    // right is a pipeline and the left fits in memory.
    if let JoinAlgo::Hash {
        left_keys,
        right_keys,
        build_left: true,
        ..
    } = algo
        && matches!(kind, JoinType::Semi | JoinType::Anti)
        && crate::vector::probes(right)
    {
        match stash(build(left, ctx, outer)?)? {
            crate::spill::Stored::Memory(rows) => {
                return crate::vector::existence_by_left(
                    rows,
                    left.width(),
                    right,
                    left_keys,
                    right_keys,
                    kind,
                    on,
                    ctx,
                    outer,
                );
            }
            stored => {
                let left_rows = std::rc::Rc::new(stored).iter();
                let right_rows = stash(build(right, ctx, outer)?)?;
                return join_with(left_rows, right_rows, spec, 0);
            }
        }
    }
    // Both sides pipelines: the table made on worker threads, when it fits.
    if let JoinAlgo::Hash {
        left_keys,
        right_keys,
        ..
    } = algo
        && made.is_none()
        && kind != JoinType::Full
        && crate::vector::probe_side(left)
        && crate::vector::probes(right)
        && let Some(table) = crate::vector::JoinTable::parallel(right, right_keys, ctx, outer)?
    {
        if !matches!(
            algo,
            JoinAlgo::Hash {
                build_left: true,
                ..
            }
        ) {
            crate::keyfilter::register(ctx, left, right, kind, left_keys, &|j| table.key_ints(j));
        }
        return probe_or_join(
            crate::vector::hash_join_table(
                left,
                std::sync::Arc::new(table),
                left_keys,
                right_keys,
                kind,
                on,
                right.width(),
                ctx,
                outer,
            )?,
            left,
            spec,
        );
    }
    // A pipeline probing a table: its right side gathered in columns, when
    // they fit.
    let gathered = match algo {
        _ if made.is_some() => made,
        JoinAlgo::Hash {
            left_keys,
            right_keys,
            build_left: false,
            ..
        } if kind != JoinType::Full && crate::vector::probe_side(left) => {
            match crate::vector::JoinTable::gather(
                build(right, ctx, outer)?,
                right.width(),
                right_keys,
                ctx,
                outer,
            )? {
                crate::vector::Gathered::Table(table) => {
                    crate::keyfilter::register(ctx, left, right, kind, left_keys, &|j| {
                        table.key_ints(j)
                    });
                    return probe_or_join(
                        crate::vector::hash_join_table(
                            left,
                            std::sync::Arc::new(table),
                            left_keys,
                            right_keys,
                            kind,
                            on,
                            right.width(),
                            ctx,
                            outer,
                        )?,
                        left,
                        spec,
                    );
                }
                crate::vector::Gathered::Stored(stored) => Some(stored),
            }
        }
        _ => None,
    };
    let right_rows = match gathered {
        Some(stored) => stored,
        None => stash(build(right, ctx, outer)?)?,
    };
    // The key filters for the left side's scans, from the right rows.
    // (Not for a join that hashes its left side: it reads the left first.)
    if let JoinAlgo::Hash {
        left_keys,
        right_keys,
        build_left: false,
        ..
    } = algo
    {
        let values = match &right_rows {
            crate::spill::Stored::Memory(rows) => crate::keyfilter::key_values(
                rows.iter().map(|r| Ok(std::borrow::Cow::Borrowed(r))),
                right_keys,
                ctx,
                outer,
            ),
            crate::spill::Stored::Run(run) => crate::keyfilter::key_values(
                std::rc::Rc::new(crate::spill::Stored::Run(run.clone()))
                    .iter()
                    .map(|r| r.map(std::borrow::Cow::Owned)),
                right_keys,
                ctx,
                outer,
            ),
        };
        crate::keyfilter::register(ctx, left, right, kind, left_keys, &|j| values[j].clone());
    }
    // A pipeline probing a table in memory: a batch at a time, in parallel.
    if let (
        JoinAlgo::Hash {
            left_keys,
            right_keys,
            ..
        },
        crate::spill::Stored::Memory(_),
    ) = (algo, &right_rows)
        && kind != JoinType::Full
        && crate::vector::probe_side(left)
    {
        let crate::spill::Stored::Memory(rows) = right_rows else {
            unreachable!("matched above")
        };
        return probe_or_join(
            crate::vector::hash_join(
                left,
                rows,
                left_keys,
                right_keys,
                kind,
                on,
                right.width(),
                ctx,
                outer,
            )?,
            left,
            spec,
        );
    }
    join_with(build(left, ctx, outer)?, right_rows, spec, 0)
}

/// A vectorized hash join's rows — or, when its left side did not become a
/// pipeline, the right rows it gave back joined a row at a time.
fn probe_or_join<'a>(
    probed: std::result::Result<Rows<'a>, Vec<Row>>,
    left: &'a Plan,
    spec: JoinSpec<'a>,
) -> Result<Rows<'a>> {
    match probed {
        Ok(rows) => Ok(rows),
        Err(right) => join_with(
            build(left, spec.ctx, spec.outer)?,
            crate::spill::Stored::Memory(right),
            spec,
            0,
        ),
    }
}

/// An index nested loop: for each left row, the right plan (a scan by an
/// index on the join keys, reading the left row as `Outer` at depth 1) run
/// for it, its rows joined as a nested loop's — on worker threads when it
/// can ([`OnWorkers`]).
fn lookup_join<'a>(left: Rows<'a>, right: &'a Plan, spec: JoinSpec<'a>) -> Result<Rows<'a>> {
    if let Some((shared, workers)) = parallel_lookups(right, &spec) {
        let (kind, on, algo, left_width, right_width) = (
            spec.kind,
            spec.on,
            spec.algo,
            spec.left_width,
            spec.right_width,
        );
        let each = move |wctx: &Ctx, l: Row| {
            let spec = JoinSpec {
                kind,
                on,
                algo,
                left_width,
                right_width,
                ctx: wctx,
                outer: None,
            };
            lookup_one(l, right, &spec)
        };
        return Ok(Box::new(OnWorkers::new(
            left,
            spec.ctx,
            shared,
            workers,
            Box::new(each),
        )));
    }
    Ok(Box::new(left.flat_map(move |l| match l {
        Ok(l) => lookup_one(l, right, &spec),
        Err(e) => vec![Err(e)],
    })))
}

/// An adaptive hash join's lookups ([`crate::plan::Adaptive`]): each of
/// its right rows `rows` looks the left side's table up, the rows put
/// back as the join gives them (its left columns first).
fn adaptive_lookups<'a>(
    rows: Vec<Row>,
    a: &'a crate::plan::Adaptive,
    spec: JoinSpec<'a>,
) -> Result<Rows<'a>> {
    static LOOKUP: JoinAlgo = JoinAlgo::Lookup;
    crate::vector::note_adaptive_lookups();
    let right_width = spec.right_width;
    let swapped = JoinSpec {
        kind: JoinType::Inner,
        on: a.on.as_ref(),
        algo: &LOOKUP,
        left_width: right_width,
        right_width: spec.left_width,
        ctx: spec.ctx,
        outer: spec.outer,
    };
    let joined = lookup_join(Box::new(rows.into_iter().map(Ok)), &a.lookup, swapped)?;
    Ok(Box::new(joined.map(move |r| {
        r.map(|mut row| {
            row.rotate_left(right_width);
            row
        })
    })))
}

/// What one left row of an index nested loop gives: its joined rows (a
/// semi or anti join's: the row itself, or nothing), or an error.
fn lookup_one(l: Row, right: &Plan, spec: &JoinSpec) -> Vec<Result<Row>> {
    // A semi or anti join: the right rows read until one matches.
    if spec.existence() {
        let e = env(&l, spec.ctx, spec.outer);
        let mut matched = false;
        match build(right, spec.ctx, Some(&e)) {
            Ok(rows) => {
                for r in rows {
                    let r = match r {
                        Ok(r) => r,
                        Err(e) => return vec![Err(e)],
                    };
                    let mut combined = l.clone();
                    combined.extend_from_slice(&r);
                    match spec.matches(&combined) {
                        Ok(true) => {
                            matched = true;
                            break;
                        }
                        Ok(false) => {}
                        Err(e) => return vec![Err(e)],
                    }
                }
            }
            Err(e) => return vec![Err(e)],
        }
        return match (spec.kind, matched) {
            (JoinType::Semi, true) | (JoinType::Anti, false) => vec![Ok(l)],
            _ => Vec::new(),
        };
    }
    let matches = match collect(right, spec.ctx, Some(&env(&l, spec.ctx, spec.outer))) {
        Ok(rows) => rows,
        Err(e) => return vec![Err(e)],
    };
    let mut out = Vec::new();
    for r in &matches {
        let mut combined = l.clone();
        combined.extend_from_slice(r);
        match spec.matches(&combined) {
            Ok(true) if spec.existence() => {
                return match spec.kind {
                    JoinType::Semi => vec![Ok(l)],
                    _ => Vec::new(),
                };
            }
            Ok(true) => out.push(Ok(combined)),
            Ok(false) => {}
            Err(e) => return vec![Err(e)],
        }
    }
    if out.is_empty()
        && let Some(row) = spec.unmatched(l)
    {
        out.push(Ok(row));
    }
    out
}

/// The first block of left rows an index nested loop looks up on worker
/// threads; each next one twice the last, up to [`LOOKUP_BLOCK_MAX`] (so a
/// query that wants few rows looks few up ahead).
const LOOKUP_BLOCK: usize = 256;
const LOOKUP_BLOCK_MAX: usize = 16_384;

/// Whether an index nested loop's lookups — or a lateral item's runs —
/// can run on worker threads, and over what: more than one worker, no
/// enclosing row, no `EXPLAIN ANALYZE`, nothing that would differ by
/// thread in the right side or the condition (a scan — by a key range, or
/// an index method's search — under filters, projections, a sort and a
/// limit, of expressions [`crate::vector::parallel_safe_in`]), no rows a
/// statement has written and a thread of its own keeps (a column table's
/// insert buffer), and a transaction that shares its reads.  Each worker
/// runs the right side whole for its left rows (it is not cut into
/// morsels; the workers do not share it out again).
fn parallel_lookups<'a>(
    right: &Plan,
    spec: &JoinSpec<'a>,
) -> Option<(&'a dyn nucleus_kv::KvRead, usize)> {
    let workers = crate::budget::workers();
    if workers < 2
        || spec.outer.is_some()
        || spec.ctx.profile.is_some()
        || crate::storage::gathered_any()
    {
        return None;
    }
    let safe = |e: &Expr| crate::vector::parallel_safe_in(e, spec.ctx);
    fn runs(p: &Plan, safe: &dyn Fn(&Expr) -> bool) -> bool {
        let mut ok = true;
        p.for_each_expr(&mut |e| ok &= safe(e));
        ok && match p {
            Plan::Scan { .. } => true,
            Plan::Project { input, .. }
            | Plan::Filter { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => runs(input, safe),
            _ => false,
        }
    }
    if !runs(right, &safe) || !spec.on.is_none_or(safe) {
        return None;
    }
    Some((spec.ctx.txn.shared()?, workers))
}

/// Rows made on worker threads, a row of the input at a time: the input
/// read a block at a time, the block cut into pieces, each piece's rows
/// given to `each` on a worker (with a context of its own), the results in
/// the input's order — a single pass's rows, and its error where it would
/// stop (the rows before it, then the error; one a consumer stops short of
/// is never seen).  An index nested loop's lookups ([`lookup_join`]), and
/// the filters and projections whose subqueries run for each row
/// ([`rows_on_workers`]).
struct OnWorkers<'a> {
    input: Rows<'a>,
    ctx: &'a Ctx<'a>,
    shared: &'a dyn nucleus_kv::KvRead,
    workers: usize,
    /// What a row of the input gives: rows, or an error.
    each: Box<dyn Fn(&Ctx, Row) -> Vec<Result<Row>> + Sync + 'a>,
    block: usize,
    out: std::collections::VecDeque<Result<Row>>,
    done: bool,
}

impl<'a> OnWorkers<'a> {
    fn new(
        input: Rows<'a>,
        ctx: &'a Ctx<'a>,
        shared: &'a dyn nucleus_kv::KvRead,
        workers: usize,
        each: Box<dyn Fn(&Ctx, Row) -> Vec<Result<Row>> + Sync + 'a>,
    ) -> Self {
        OnWorkers {
            input,
            ctx,
            shared,
            workers,
            each,
            block: LOOKUP_BLOCK,
            out: std::collections::VecDeque::new(),
            done: false,
        }
    }

    /// The next block's rows into `out`.
    fn fill(&mut self) {
        let mut rows = Vec::with_capacity(self.block);
        let mut failed = None;
        while rows.len() < self.block {
            match self.input.next() {
                None => {
                    self.done = true;
                    break;
                }
                Some(Ok(r)) => rows.push(r),
                Some(Err(e)) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        self.block = (self.block * 2).min(LOOKUP_BLOCK_MAX);
        if !rows.is_empty() {
            crate::vector::note_parallel_run();
            let pieces = (self.workers * 4).min(rows.len());
            let per = rows.len().div_ceil(pieces);
            let chunks: Vec<&[Row]> = rows.chunks(per).collect();
            let each = &self.each;
            let results = crate::vector::on_workers(
                self.ctx,
                self.shared,
                self.workers,
                chunks.len(),
                false,
                &|wctx, m| {
                    let mut out = Vec::new();
                    for r in chunks[m] {
                        let got = each(wctx, r.clone());
                        let failed = got.iter().any(Result::is_err);
                        out.extend(got);
                        if failed {
                            break;
                        }
                    }
                    Ok(out)
                },
            );
            for r in results {
                let got = match r {
                    Some(Ok(got)) => got,
                    Some(Err(e)) => vec![Err(e)],
                    None => vec![Err(Error::internal("a piece of rows not run"))],
                };
                for row in got {
                    let failed = row.is_err();
                    self.out.push_back(row);
                    if failed {
                        self.done = true;
                        return;
                    }
                }
            }
        }
        if let Some(e) = failed {
            self.out.push_back(Err(e));
            self.done = true;
        }
    }
}

impl Iterator for OnWorkers<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if let Some(r) = self.out.pop_front() {
                return Some(r);
            }
            if self.done {
                return None;
            }
            self.fill();
        }
    }
}

/// Whether a filter's or projection's expressions `exprs` go to worker
/// threads ([`OnWorkers`]), and over what: a subquery among them that runs
/// for each row (correlated — the work that pays for the threads), nothing
/// that would differ by thread ([`crate::vector::parallel_safe_in`]), more
/// than one worker, no enclosing row, no `EXPLAIN ANALYZE`, and a
/// transaction that shares its reads.
fn rows_on_workers<'a>(
    exprs: &[&Expr],
    ctx: &'a Ctx<'a>,
    outer: Option<&Env>,
) -> Option<(&'a dyn nucleus_kv::KvRead, usize)> {
    let workers = crate::budget::workers();
    if workers < 2 || outer.is_some() || ctx.profile.is_some() {
        return None;
    }
    let mut correlated = false;
    for e in exprs {
        e.walk(&mut |x| correlated |= matches!(x, Expr::Subquery(s) if s.correlated));
    }
    if !correlated
        || !exprs
            .iter()
            .all(|e| crate::vector::parallel_safe_in(e, ctx))
    {
        return None;
    }
    Some((ctx.txn.shared()?, workers))
}

fn join_with<'a>(
    left: Rows<'a>,
    right: crate::spill::Stored,
    spec: JoinSpec<'a>,
    level: u32,
) -> Result<Rows<'a>> {
    match right {
        crate::spill::Stored::Memory(rows) => memory_join(left, rows, spec),
        crate::spill::Stored::Run(run) => match spec.keys() {
            Some(keys) if level < crate::spill::MAX_LEVEL => {
                grace_join(left, run, keys, spec, level)
            }
            // (`on` holds the whole condition: keys or not, it decides.)
            _ => block_join(left, run, spec),
        },
    }
}

/// The right rows of a hash join by key (rows with a NULL key left out:
/// NULL never equals anything).
fn hash_table(
    rows: &[Row],
    keys: &[Expr],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<crate::fasthash::FastMap<Vec<Value>, Vec<usize>>> {
    let mut table: crate::fasthash::FastMap<Vec<Value>, Vec<usize>> = Default::default();
    for (i, r) in rows.iter().enumerate() {
        if let Some(key) = probe_key(r, keys, ctx, outer)? {
            table.entry(key).or_default().push(i);
        }
    }
    Ok(table)
}

/// A row's hash join key; `None` if it has a NULL.
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

/// The right side in memory: each left row probes it (by key, or every
/// row); a full join then gives the right rows nothing matched.
fn memory_join<'a>(left: Rows<'a>, right: Vec<Row>, spec: JoinSpec<'a>) -> Result<Rows<'a>> {
    let table = match spec.keys() {
        Some((_, right_keys)) => Some(hash_table(&right, right_keys, spec.ctx, spec.outer)?),
        None => None,
    };
    let right = Rc::new(right);
    let full = spec.kind == JoinType::Full;
    let matched = Rc::new(std::cell::RefCell::new(vec![
        false;
        if full {
            right.len()
        } else {
            0
        }
    ]));
    let (r, m) = (Rc::clone(&right), Rc::clone(&matched));
    let probe = left.flat_map(move |l| -> Vec<Result<Row>> {
        let l = match l {
            Ok(l) => l,
            Err(e) => return vec![Err(e)],
        };
        let by_key: Vec<usize>;
        let candidates: Box<dyn Iterator<Item = usize>> = match (&table, spec.keys()) {
            (Some(t), Some((left_keys, _))) => {
                match probe_key(&l, left_keys, spec.ctx, spec.outer) {
                    Ok(Some(k)) => {
                        by_key = t.get(&k).cloned().unwrap_or_default();
                        Box::new(by_key.into_iter())
                    }
                    Ok(None) => Box::new(std::iter::empty()),
                    Err(e) => return vec![Err(e)],
                }
            }
            _ => Box::new(0..r.len()),
        };
        let mut out = Vec::new();
        for i in candidates {
            let mut combined = l.clone();
            combined.extend_from_slice(&r[i]);
            match spec.matches(&combined) {
                Ok(true) if spec.existence() => {
                    return match spec.kind {
                        JoinType::Semi => vec![Ok(l)],
                        _ => Vec::new(),
                    };
                }
                Ok(true) => {
                    if full {
                        m.borrow_mut()[i] = true;
                    }
                    out.push(Ok(combined));
                }
                Ok(false) => {}
                Err(e) => return vec![Err(e)],
            }
        }
        if out.is_empty()
            && let Some(row) = spec.unmatched(l)
        {
            out.push(Ok(row));
        }
        out
    });
    if !full {
        return Ok(Box::new(probe));
    }
    // After every left row: the right rows nothing matched.
    let mut tail = None;
    let unmatched = std::iter::from_fn(move || {
        let it = tail.get_or_insert_with(|| {
            let m = matched.borrow();
            (0..right.len())
                .filter(|&i| !m[i])
                .map(|i| Ok(spec.right_padded(&right[i])))
                .collect::<Vec<_>>()
                .into_iter()
        });
        it.next()
    });
    Ok(Box::new(probe.chain(unmatched)))
}

/// A grace hash join: both sides partitioned by key (rows with a NULL key
/// match nothing: a left one comes out padded for LEFT and FULL, a right
/// one for FULL), then each pair of partitions joined, in memory if its
/// right side fits.
fn grace_join<'a>(
    left: Rows<'a>,
    right: std::sync::Arc<crate::spill::Sealed>,
    (left_keys, right_keys): (&'a [Expr], &'a [Expr]),
    spec: JoinSpec<'a>,
    level: u32,
) -> Result<Rows<'a>> {
    let (ctx, outer) = (spec.ctx, spec.outer);
    let mut right_parts = crate::spill::Partitions::new(level);
    let mut right_nulls = crate::spill::Stash::new();
    for r in right.iter() {
        let r = r?;
        match probe_key(&r, right_keys, ctx, outer)? {
            Some(k) => right_parts.push(&k, &r)?,
            None if spec.kind == JoinType::Full => right_nulls.push(spec.right_padded(&r))?,
            None => {}
        }
    }
    let mut left_parts = crate::spill::Partitions::new(level);
    let mut left_nulls = crate::spill::Stash::new();
    for l in left {
        let l = l?;
        match probe_key(&l, left_keys, ctx, outer)? {
            Some(k) => left_parts.push(&k, &l)?,
            None => {
                if let Some(row) = spec.unmatched(l) {
                    left_nulls.push(row)?;
                }
            }
        }
    }
    let pairs: Vec<_> = left_parts
        .seal_all()?
        .into_iter()
        .zip(right_parts.seal_all()?)
        .collect();
    let joined = pairs.into_iter().flat_map(move |(l, r)| -> Rows<'a> {
        let left: Rows<'a> = match l {
            Some(run) => Box::new(run.iter()),
            None => Box::new(std::iter::empty()),
        };
        let result = (|| {
            let right = match r {
                Some(run) => stash(Box::new(run.iter()))?,
                None => crate::spill::Stored::Memory(Vec::new()),
            };
            join_with(left, right, spec, level + 1)
        })();
        match result {
            Ok(rows) => rows,
            Err(e) => Box::new(std::iter::once(Err(e))),
        }
    });
    Ok(Box::new(
        joined.chain(left_nulls.seal()?).chain(right_nulls.seal()?),
    ))
}

/// A block nested loop: left rows a work_mem's worth at a time, each block
/// read against every right row; matched right rows remembered for a full
/// join, whose unmatched right rows come after.
fn block_join<'a>(
    mut left: Rows<'a>,
    right: std::sync::Arc<crate::spill::Sealed>,
    spec: JoinSpec<'a>,
) -> Result<Rows<'a>> {
    let limit = crate::budget::work_mem();
    let full = spec.kind == JoinType::Full;
    let mut matched = vec![
        0u64;
        if full {
            (right.rows() as usize).div_ceil(64)
        } else {
            0
        }
    ];
    let mut out = crate::spill::Stash::new();
    loop {
        let mut block = Vec::new();
        let mut bytes = 0;
        for l in left.by_ref() {
            let l = l?;
            bytes += crate::spill::row_bytes(&l);
            block.push(l);
            if bytes > limit {
                break;
            }
        }
        if block.is_empty() {
            break;
        }
        let mut hit = vec![false; block.len()];
        for (i, r) in right.iter().enumerate() {
            let r = r?;
            for (j, l) in block.iter().enumerate() {
                if hit[j] && spec.existence() {
                    continue;
                }
                let mut combined = l.clone();
                combined.extend_from_slice(&r);
                if spec.matches(&combined)? {
                    hit[j] = true;
                    if full {
                        matched[i / 64] |= 1 << (i % 64);
                    }
                    if !spec.existence() {
                        out.push(combined)?;
                    }
                }
            }
        }
        for (l, h) in block.into_iter().zip(hit) {
            if h && spec.kind == JoinType::Semi {
                out.push(l)?;
            } else if !h && let Some(row) = spec.unmatched(l) {
                out.push(row)?;
            }
        }
    }
    if full {
        for (i, r) in right.iter().enumerate() {
            if matched[i / 64] & (1 << (i % 64)) == 0 {
                out.push(spec.right_padded(&r?))?;
            }
        }
    }
    Ok(Box::new(out.seal()?.into_iter()))
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

/// Adds a row's arguments to an aggregate (or to its `ORDER BY` buffer),
/// counting the memory a buffer or a `DISTINCT` set grows by.
fn feed_row(agg: &Agg, st: &mut AggState, e: &Env, bytes: &mut usize) -> Result<()> {
    let args = agg
        .args
        .iter()
        .map(|a| a.eval(e))
        .collect::<Result<Vec<_>>>()?;
    if let Some(buffer) = &mut st.buffer {
        let keys = agg
            .order
            .iter()
            .map(|k| k.expr.eval(e))
            .collect::<Result<Vec<_>>>()?;
        *bytes += crate::spill::row_bytes(&keys) + crate::spill::row_bytes(&args);
        buffer.push((keys, args));
        return Ok(());
    }
    if st.seen.is_some() {
        *bytes += crate::spill::row_bytes(&args);
    }
    feed(agg, st, args)
}

/// Groups and their aggregates.  The groups are kept in memory up to
/// work_mem; past it, rows of groups already there still go to them, and
/// rows of new groups to partitions (by group key), each aggregated in
/// turn after the input — so no accumulator ever leaves memory.
pub(crate) fn aggregate<'a>(
    rows: Rows<'a>,
    group: &'a [Expr],
    aggs: &'a [Agg],
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
    level: u32,
) -> Result<Rows<'a>> {
    let limit = crate::budget::work_mem();
    let mut bytes = 0;
    let mut parts: Option<crate::spill::Partitions> = None;
    let fresh = || -> Vec<AggState> {
        aggs.iter()
            .map(|a| AggState {
                acc: a.func.start(),
                seen: a.distinct.then(HashSet::new),
                buffer: (!a.order.is_empty()).then(Vec::new),
            })
            .collect()
    };
    let mut index: crate::fasthash::FastMap<Vec<Value>, usize> = Default::default();
    let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
    for r in rows {
        let row = r?;
        let e = Env {
            row: &row,
            outer,
            ctx,
        };
        // A grand total is one group: no key to hash.
        if group.is_empty() && !groups.is_empty() {
            for (agg, st) in aggs.iter().zip(groups[0].1.iter_mut()) {
                feed_row(agg, st, &e, &mut bytes)?;
            }
            continue;
        }
        let key = group
            .iter()
            .map(|g| g.eval(&e))
            .collect::<Result<Vec<_>>>()?;
        let gi = match index.get(&key) {
            Some(&i) => i,
            None => {
                let size = crate::spill::row_bytes(&key) + 96 * aggs.len();
                // (A grand total is one group: it never partitions.)
                if parts.is_some()
                    || (bytes + size > limit
                        && level < crate::spill::MAX_LEVEL
                        && !group.is_empty())
                {
                    parts
                        .get_or_insert_with(|| crate::spill::Partitions::new(level))
                        .push(&key, &row)?;
                    continue;
                }
                bytes += size;
                index.insert(key.clone(), groups.len());
                groups.push((key, fresh()));
                groups.len() - 1
            }
        };
        for (agg, st) in aggs.iter().zip(groups[gi].1.iter_mut()) {
            feed_row(agg, st, &e, &mut bytes)?;
        }
    }
    if groups.is_empty() && group.is_empty() && level == 0 {
        groups.push((Vec::new(), fresh())); // aggregates over no rows
    }
    let done: Vec<Row> = groups
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
        .collect::<Result<_>>()?;
    let runs = match parts {
        Some(p) => p.seal()?,
        None => Vec::new(),
    };
    Ok(Box::new(done.into_iter().map(Ok).chain(
        runs.into_iter().flat_map(move |run| {
            match aggregate(Box::new(run.iter()), group, aggs, ctx, outer, level + 1) {
                Ok(rows) => rows,
                Err(e) => Box::new(std::iter::once(Err(e))),
            }
        }),
    )))
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

/// The index entry key for index values `vals` (the columns' values, or
/// their mappings in taught orderings) in `table.indexes[i]`.
pub fn index_key(table: &Table, i: usize, vals: Vec<Value>, pk_bytes: &[u8]) -> (Vec<u8>, bool) {
    let index = &table.indexes[i];
    let all_set = !vals.iter().any(Value::is_null);
    // One allocation: the prefix, the values (most a tag and eight bytes)
    // and the primary key.
    let mut k = Vec::with_capacity(9 + 9 * vals.len() + pk_bytes.len());
    table.push_index_prefix(&mut k, index.id);
    for v in &vals {
        crate::encoding::encode_key_value(&mut k, v);
    }
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
