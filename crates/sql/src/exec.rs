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
use crate::encoding::{decode_row, encode_key, encode_key_value};
use crate::error::{Error, Result};
use crate::expr::{Ctx, Env, Expr};
use crate::plan::{Access, Agg, AggFunc, JoinAlgo, JoinType, Plan, SortKey};
use crate::types::{DataType, Row, Value};

/// A stream of rows.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// Runs `plan` to completion.
pub fn collect(plan: &Plan, ctx: &Ctx, outer: Option<&Env>) -> Result<Vec<Row>> {
    build(plan, ctx, outer)?.collect()
}

fn env<'a>(row: &'a [Value], ctx: &'a Ctx<'a>, outer: Option<&'a Env<'a>>) -> Env<'a> {
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
        Plan::Filter { input, predicate } => {
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

/// Rows shared behind an `Rc`, cloned as they are read.
fn shared<'a>(rows: Rc<Vec<Row>>) -> Rows<'a> {
    Box::new((0..rows.len()).map(move |i| Ok(rows[i].clone())))
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
    // A bound of the wrong type would encode in the wrong place: then read
    // more (the filter decides), never less.
    let fits = |v: &Value, pos: usize| {
        v.is_null()
            || v.data_type() == ty(pos)
            || (v.data_type() == Some(DataType::Int) && ty(pos) == Some(DataType::Float))
    };
    let coerce = |v: Value, pos: usize| -> Value {
        match (v, ty(pos)) {
            (Value::Int(i), Some(DataType::Float)) => Value::Float(i as f64),
            (v, _) => v,
        }
    };
    let mut usable = true;
    for (pos, p) in prefix.iter().enumerate() {
        let v = p.eval(e)?;
        if v.is_null() {
            return Ok(None); // `col = NULL` matches nothing
        }
        if !fits(&v, pos) {
            usable = false;
            break;
        }
        encode_key_value(&mut key, &coerce(v, pos));
    }
    let base_end = prefix_end(&key);
    if !usable {
        return Ok(Some((key.clone(), base_end, *index)));
    }
    let pos = prefix.len();
    let bound_key = |b: &Expr| -> Result<Option<Vec<u8>>> {
        let v = b.eval(e)?;
        if v.is_null() || !fits(&v, pos) {
            return Ok(None);
        }
        let mut k = key.clone();
        encode_key_value(&mut k, &coerce(v, pos));
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
    let entries = ctx.txn.scan(Bound::Included(&start), end_bound)?;
    let ncols = table.columns.len();
    Ok(match index {
        None => Box::new(entries.map(move |e| {
            let (k, v) = e?;
            Ok((k, decode_row(&v, ncols)?))
        })),
        Some(_) => {
            let base = table.index_prefix(0);
            Box::new(entries.map(move |e| {
                let (_, pk) = e?;
                let mut k = base.clone();
                k.extend_from_slice(&pk);
                let v = ctx
                    .txn
                    .get(&k)?
                    .ok_or_else(|| Error::internal("index entry without its row"))?;
                Ok((k, decode_row(&v, ncols)?))
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

enum Acc {
    Count(i64),
    Sum(Value),
    Avg(f64, i64),
    MinMax(Value),
}

fn aggregate(
    rows: Rows,
    group: &[Expr],
    aggs: &[Agg],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Result<Vec<Row>> {
    let fresh = || -> Vec<(Acc, Option<HashSet<Value>>)> {
        aggs.iter()
            .map(|a| {
                let acc = match a.func {
                    AggFunc::Count => Acc::Count(0),
                    AggFunc::Sum => Acc::Sum(Value::Null),
                    AggFunc::Avg => Acc::Avg(0.0, 0),
                    AggFunc::Min | AggFunc::Max => Acc::MinMax(Value::Null),
                };
                (acc, a.distinct.then(HashSet::new))
            })
            .collect()
    };
    let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<(Acc, Option<HashSet<Value>>)>)> = Vec::new();
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
        for (agg, (acc, seen)) in aggs.iter().zip(groups[gi].1.iter_mut()) {
            let v = match &agg.arg {
                None => Value::Bool(true), // count(*)
                Some(a) => a.eval(&e)?,
            };
            if v.is_null() {
                continue;
            }
            if let Some(seen) = seen
                && !seen.insert(v.clone())
            {
                continue;
            }
            accumulate(agg.func, acc, v)?;
        }
    }
    if groups.is_empty() && group.is_empty() {
        groups.push((Vec::new(), fresh())); // aggregates over no rows
    }
    Ok(groups
        .into_iter()
        .map(|(mut key, accs)| {
            key.extend(accs.into_iter().map(|(acc, _)| match acc {
                Acc::Count(n) => Value::Int(n),
                Acc::Sum(v) | Acc::MinMax(v) => v,
                Acc::Avg(_, 0) => Value::Null,
                Acc::Avg(s, n) => Value::Float(s / n as f64),
            }));
            key
        })
        .collect())
}

fn accumulate(func: AggFunc, acc: &mut Acc, v: Value) -> Result<()> {
    match acc {
        Acc::Count(n) => *n += 1,
        Acc::Sum(s) => {
            *s = if s.is_null() {
                match v {
                    Value::Int(_) | Value::Float(_) => v,
                    v => {
                        return Err(Error::undefined_function(format!(
                            "function sum({}) does not exist",
                            crate::types::type_name(&v)
                        )));
                    }
                }
            } else {
                crate::expr::binary(crate::ast::BinaryOp::Add, s.clone(), v)?
            }
        }
        Acc::Avg(s, n) => match v {
            Value::Int(i) => {
                *s += i as f64;
                *n += 1;
            }
            Value::Float(f) => {
                *s += f;
                *n += 1;
            }
            v => {
                return Err(Error::undefined_function(format!(
                    "function avg({}) does not exist",
                    crate::types::type_name(&v)
                )));
            }
        },
        Acc::MinMax(m) => {
            let replace = m.is_null() || {
                let o = v.sql_cmp(m)?.expect("not null");
                if func == AggFunc::Min {
                    o.is_lt()
                } else {
                    o.is_gt()
                }
            };
            if replace {
                *m = v;
            }
        }
    }
    Ok(())
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
    keyed.sort_by(|(a, _), (b, _)| {
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
    });
    Ok(keyed.into_iter().map(|(_, r)| r).collect())
}

/// The encoded primary key of a row (hidden row ids passed in `rowid`).
pub fn row_key_of(table: &Table, row: &[Value], rowid: Option<i64>) -> Vec<u8> {
    match rowid {
        Some(id) => row_key(table, &[Value::Int(id)]),
        None => row_key(table, &pk_of(table, row)),
    }
}

/// The index entry key of `row` in `table.indexes[i]`, and whether its
/// index columns are all non-NULL (only those take part in uniqueness).
pub fn index_entry(table: &Table, i: usize, row: &[Value], pk_bytes: &[u8]) -> (Vec<u8>, bool) {
    let index = &table.indexes[i];
    let vals: Vec<Value> = index.columns.iter().map(|&c| row[c].clone()).collect();
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
