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
            let rows = scan_some(table, access, ctx, outer, wanted.map(Vec::as_slice))?;
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
        Plan::Aggregate { input, group, aggs } => {
            aggregate(build(input, ctx, outer)?, group, aggs, ctx, outer, 0)?
        }
        Plan::Sort { input, keys } => {
            // A pipeline's rows: sorted in parallel, morsels merged.
            if let Some(rows) = crate::vector::sorted(input, keys, ctx, outer)? {
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
            let rows = build(input, ctx, outer)?.skip(offset);
            match limit {
                Some(n) => Box::new(rows.take(n)),
                None => Box::new(rows),
            }
        }
        Plan::Distinct { input } => Box::new(Distinct::new(build(input, ctx, outer)?, 0)),
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
    pending: std::collections::VecDeque<Rc<crate::spill::Sealed>>,
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
    let side = |run: Option<Rc<crate::spill::Sealed>>| -> Rows<'static> {
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
    let mut s = crate::spill::Stash::new();
    for r in matching(table, access, filter, ctx)? {
        let (k, row) = r?;
        s.push(keyed(k, row))?;
    }
    s.seal()
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
pub fn pk_suffix(table: &Table, key: &[u8]) -> Vec<u8> {
    key[table.index_prefix(0).len()..].to_vec()
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
    let right_rows = stash(build(right, ctx, outer)?)?;
    // A pipeline probing a table in memory: a batch at a time, in parallel.
    if let (
        JoinAlgo::Hash {
            left_keys,
            right_keys,
        },
        crate::spill::Stored::Memory(_),
    ) = (algo, &right_rows)
        && kind != JoinType::Full
        && crate::vector::probes(left)
    {
        let crate::spill::Stored::Memory(rows) = right_rows else {
            unreachable!("matched above")
        };
        return crate::vector::hash_join(
            left,
            rows,
            left_keys,
            right_keys,
            kind,
            on,
            right.width(),
            ctx,
            outer,
        );
    }
    join_with(build(left, ctx, outer)?, right_rows, spec, 0)
}

/// An index nested loop: for each left row, the right plan (a scan by an
/// index on the join keys, reading the left row as `Outer` at depth 1) run
/// for it, its rows joined as a nested loop's.
fn lookup_join<'a>(left: Rows<'a>, right: &'a Plan, spec: JoinSpec<'a>) -> Result<Rows<'a>> {
    Ok(Box::new(left.flat_map(move |l| -> Vec<Result<Row>> {
        let l = match l {
            Ok(l) => l,
            Err(e) => return vec![Err(e)],
        };
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
    })))
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
    right: Rc<crate::spill::Sealed>,
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
    right: Rc<crate::spill::Sealed>,
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
