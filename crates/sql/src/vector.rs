//! Vectorized execution (PLAN.md §M14.20 V2): operators that pass batches
//! of rows — a typed vector per column and a selection of the rows still
//! in play — instead of one row of values at a time.
//!
//! A *pipeline* runs a table scan and the filters and projections above it
//! a batch at a time; its batches go to a vectorized aggregate, or become
//! rows for the operators above.  Expressions are evaluated a batch per
//! call: typed kernels for the built-in types' arithmetic, comparisons and
//! logic, a value at a time for everything else (so nothing is refused,
//! only slower).  Aggregates are fed a batch per call through
//! [`GroupAccumulator`] — a capability; one without it is fed row by row.
//!
//! The result is the row-at-a-time executor's, errors included: an
//! evaluation that fails on a batch is run again a row at a time, which
//! yields the rows before the first failing one and then its error — or,
//! should the batch have failed where rows would not (an operand
//! evaluated that a row would have skipped), the batch's rows.  `AND` and
//! `OR` evaluate their right side only on the rows that need it, so this
//! is rare.  `batch_size = 0` turns the executor off (the test kit runs
//! both ways).

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::ast::{BinaryOp, UnaryOp};
use crate::catalog::Table;
use crate::encoding::{OUT_OF_LINE, Reader};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::exec::{Entries, Rows, env};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::{Accumulator, AggRef, CallContext, GroupAccumulator};
use crate::kernels::{self, Arith, Cmp, FArith, MixState, Rhs};
use crate::plan::{Access, Agg, Plan};
use crate::types::{DataType, Row, Value};

/// The first batch of a pipeline: small, so a query that needs only its
/// first rows reads few ahead; each next one twice the last, up to
/// `batch_size`.
const FIRST_BATCH: usize = 64;

/// A column of a batch.  Positions outside the batch's selection hold
/// arbitrary values.
#[derive(Clone, Debug, PartialEq)]
pub enum Vector {
    /// The same value in every row.
    Const(Value),
    /// Integers, and which are `NULL` (`None`: none).
    Int(Vec<i64>, Option<Vec<bool>>),
    /// Doubles, and which are `NULL`.
    Float(Vec<f64>, Option<Vec<bool>>),
    /// Booleans, and which are `NULL`.
    Bool(Vec<bool>, Option<Vec<bool>>),
    /// Text: row `i`'s bytes `bytes[offsets[i]..offsets[i + 1]]` (UTF-8),
    /// and which are `NULL`.
    Text(Vec<u32>, Vec<u8>, Option<Vec<bool>>),
    /// Dates, times or timestamps as their integers (days or µs), and which
    /// are `NULL`.
    Time(TimeKind, Vec<i64>, Option<Vec<bool>>),
    /// Any values.
    Values(Vec<Value>),
}

/// Which kind of date / time a [`Vector::Time`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeKind {
    /// `date`.
    Date,
    /// `time`.
    Time,
    /// `timestamp`.
    Timestamp,
    /// `timestamptz`.
    TimestampTz,
}

impl TimeKind {
    fn of(v: &Value) -> Option<(TimeKind, i64)> {
        Some(match v {
            Value::Date(d) => (TimeKind::Date, i64::from(*d)),
            Value::Time(t) => (TimeKind::Time, *t),
            Value::Timestamp(t) => (TimeKind::Timestamp, *t),
            Value::TimestampTz(t) => (TimeKind::TimestampTz, *t),
            _ => return None,
        })
    }

    /// The value of this kind an integer stands for.
    pub(crate) fn value(self, x: i64) -> Value {
        match self {
            TimeKind::Date => Value::Date(x as i32),
            TimeKind::Time => Value::Time(x),
            TimeKind::Timestamp => Value::Timestamp(x),
            TimeKind::TimestampTz => Value::TimestampTz(x),
        }
    }
}

fn is_null_at(nulls: &Option<Vec<bool>>, i: usize) -> bool {
    nulls.as_ref().is_some_and(|n| n[i])
}

/// Which rows are `NULL` (`None`: none).
fn nulls_from(n: Vec<bool>) -> Option<Vec<bool>> {
    n.contains(&true).then_some(n)
}

impl Vector {
    /// Row `i`'s value.
    pub fn value(&self, i: usize) -> Value {
        match self {
            Vector::Const(v) => v.clone(),
            Vector::Int(d, n) if !is_null_at(n, i) => Value::Int(d[i]),
            Vector::Float(d, n) if !is_null_at(n, i) => Value::Float(d[i]),
            Vector::Bool(d, n) if !is_null_at(n, i) => Value::Bool(d[i]),
            Vector::Text(o, b, n) if !is_null_at(n, i) => Value::Text(
                String::from_utf8_lossy(&b[o[i] as usize..o[i + 1] as usize]).into_owned(),
            ),
            Vector::Time(k, d, n) if !is_null_at(n, i) => k.value(d[i]),
            Vector::Values(v) => v[i].clone(),
            _ => Value::Null,
        }
    }

    /// Whether row `i` is `NULL`.
    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Vector::Const(v) => v.is_null(),
            Vector::Int(_, n)
            | Vector::Float(_, n)
            | Vector::Bool(_, n)
            | Vector::Text(_, _, n)
            | Vector::Time(_, _, n) => is_null_at(n, i),
            Vector::Values(v) => v[i].is_null(),
        }
    }

    /// Rows `range` of it, as a vector of their own.
    pub fn slice(&self, range: std::ops::Range<usize>) -> Vector {
        let nulls = |n: &Option<Vec<bool>>| {
            n.as_ref()
                .and_then(|n| nulls_from(n[range.clone()].to_vec()))
        };
        match self {
            Vector::Const(v) => Vector::Const(v.clone()),
            Vector::Int(d, n) => Vector::Int(d[range.clone()].to_vec(), nulls(n)),
            Vector::Float(d, n) => Vector::Float(d[range.clone()].to_vec(), nulls(n)),
            Vector::Bool(d, n) => Vector::Bool(d[range.clone()].to_vec(), nulls(n)),
            Vector::Time(k, d, n) => Vector::Time(*k, d[range.clone()].to_vec(), nulls(n)),
            Vector::Text(o, b, n) => {
                let (lo, hi) = (o[range.start] as usize, o[range.end] as usize);
                let offsets = o[range.start..=range.end]
                    .iter()
                    .map(|&x| x - lo as u32)
                    .collect();
                Vector::Text(offsets, b[lo..hi].to_vec(), nulls(n))
            }
            Vector::Values(v) => Vector::Values(v[range].to_vec()),
        }
    }

    /// Rows `sel` of it, in order, as a vector of their own.
    pub fn gather(&self, sel: &Sel) -> Vector {
        let rows = match sel {
            Sel::All(n) => return self.slice(0..*n),
            Sel::Rows(r) => r,
        };
        let nulls = |n: &Option<Vec<bool>>| {
            n.as_ref()
                .and_then(|n| nulls_from(rows.iter().map(|&i| n[i as usize]).collect()))
        };
        fn pick<T: Copy>(d: &[T], rows: &[u32]) -> Vec<T> {
            rows.iter().map(|&i| d[i as usize]).collect()
        }
        match self {
            Vector::Const(v) => Vector::Const(v.clone()),
            Vector::Int(d, n) => Vector::Int(pick(d, rows), nulls(n)),
            Vector::Float(d, n) => Vector::Float(pick(d, rows), nulls(n)),
            Vector::Bool(d, n) => Vector::Bool(pick(d, rows), nulls(n)),
            Vector::Time(k, d, n) => Vector::Time(*k, pick(d, rows), nulls(n)),
            Vector::Text(o, b, n) => {
                let mut offsets = Vec::with_capacity(rows.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0);
                for &i in rows {
                    let i = i as usize;
                    bytes.extend_from_slice(&b[o[i] as usize..o[i + 1] as usize]);
                    offsets.push(bytes.len() as u32);
                }
                Vector::Text(offsets, bytes, nulls(n))
            }
            Vector::Values(v) => {
                Vector::Values(rows.iter().map(|&i| v[i as usize].clone()).collect())
            }
        }
    }

    /// Which rows are `NULL`, for a typed vector (`None`: none, or not
    /// typed).
    pub fn nulls(&self) -> Option<&[bool]> {
        match self {
            Vector::Int(_, n)
            | Vector::Float(_, n)
            | Vector::Bool(_, n)
            | Vector::Text(_, _, n)
            | Vector::Time(_, _, n) => n.as_deref(),
            _ => None,
        }
    }

    /// `values` as a typed vector when they are all of one kind a vector
    /// types (or `NULL`).
    pub fn typed(values: Vec<Value>) -> Vector {
        let nulls = |values: &[Value]| nulls_from(values.iter().map(Value::is_null).collect());
        let all = |f: fn(&Value) -> bool| values.iter().all(|v| v.is_null() || f(v));
        match values.iter().find(|v| !v.is_null()) {
            Some(Value::Int(_)) if all(|v| matches!(v, Value::Int(_))) => {
                let data = values
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                    .collect();
                Vector::Int(data, nulls(&values))
            }
            Some(Value::Float(_)) if all(|v| matches!(v, Value::Float(_))) => {
                let data = values
                    .iter()
                    .map(|v| if let Value::Float(f) = v { *f } else { 0.0 })
                    .collect();
                Vector::Float(data, nulls(&values))
            }
            Some(Value::Bool(_)) if all(|v| matches!(v, Value::Bool(_))) => {
                let data = values
                    .iter()
                    .map(|v| matches!(v, Value::Bool(true)))
                    .collect();
                Vector::Bool(data, nulls(&values))
            }
            Some(Value::Text(_)) if all(|v| matches!(v, Value::Text(_))) => {
                let mut offsets = Vec::with_capacity(values.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0);
                for v in &values {
                    if let Value::Text(s) = v {
                        bytes.extend_from_slice(s.as_bytes());
                    }
                    offsets.push(bytes.len() as u32);
                }
                Vector::Text(offsets, bytes, nulls(&values))
            }
            Some(first) if TimeKind::of(first).is_some() => {
                let kind = TimeKind::of(first).expect("a time").0;
                if !values
                    .iter()
                    .all(|v| v.is_null() || TimeKind::of(v).is_some_and(|(k, _)| k == kind))
                {
                    return Vector::Values(values);
                }
                let data = values
                    .iter()
                    .map(|v| TimeKind::of(v).map_or(0, |(_, x)| x))
                    .collect();
                Vector::Time(kind, data, nulls(&values))
            }
            _ => Vector::Values(values),
        }
    }
}

/// The rows of a batch in play.
#[derive(Clone, Debug, PartialEq)]
pub enum Sel {
    /// Rows `0..n`.
    All(usize),
    /// These rows, ascending.
    Rows(Vec<u32>),
}

impl Sel {
    /// How many.
    pub fn len(&self) -> usize {
        match self {
            Sel::All(n) => *n,
            Sel::Rows(r) => r.len(),
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `k`-th row in play.
    pub fn nth(&self, k: usize) -> usize {
        match self {
            Sel::All(_) => k,
            Sel::Rows(r) => r[k] as usize,
        }
    }

    /// The rows in play, ascending.
    pub fn iter(&self) -> SelIter<'_> {
        match self {
            Sel::All(n) => SelIter::All(0..*n),
            Sel::Rows(r) => SelIter::Rows(r.iter()),
        }
    }

    /// The rows of these for which `keep` holds.
    fn keep(&self, mut keep: impl FnMut(usize) -> bool) -> Sel {
        Sel::Rows(self.iter().filter(|&i| keep(i)).map(|i| i as u32).collect())
    }
}

/// [`Sel::iter`].
pub enum SelIter<'a> {
    /// A range.
    All(std::ops::Range<usize>),
    /// A list.
    Rows(std::slice::Iter<'a, u32>),
}

impl Iterator for SelIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            SelIter::All(r) => r.next(),
            SelIter::Rows(r) => r.next().map(|&i| i as usize),
        }
    }
}

/// Rows as columns: `len` positions, of which `sel` are in play.
#[derive(Clone, Debug)]
pub struct Batch {
    /// Positions.
    pub len: usize,
    /// A vector per column (a column nothing reads is a `NULL` constant).
    pub cols: Vec<Rc<Vector>>,
    /// The rows in play.
    pub sel: Sel,
}

impl Batch {
    /// Row `i` as values.
    pub fn row(&self, i: usize) -> Row {
        self.cols.iter().map(|c| c.value(i)).collect()
    }
}

/// What expressions are evaluated in besides the batch.
#[derive(Clone, Copy)]
struct Cx<'a> {
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
}

impl<'a> Cx<'a> {
    fn env(&self, row: &'a [Value]) -> Env<'a> {
        env(row, self.ctx, self.outer)
    }
}

// ------------------------------------------------------------ expressions

/// `e` over the rows `sel` of `b`.
fn eval(e: &Expr, b: &Batch, sel: &Sel, cx: Cx) -> Result<Rc<Vector>> {
    if sel.is_empty() {
        return Ok(Rc::new(Vector::Const(Value::Null)));
    }
    Ok(match e {
        Expr::Literal(v) => Rc::new(Vector::Const(v.clone())),
        Expr::Param(_) | Expr::Outer { .. } => Rc::new(Vector::Const(e.eval(&cx.env(&[]))?)),
        Expr::Column(i) => b
            .cols
            .get(*i)
            .cloned()
            .ok_or_else(|| Error::internal("column out of range"))?,
        Expr::Binary(op @ (BinaryOp::And | BinaryOp::Or), l, r) => {
            Rc::new(logic(*op == BinaryOp::And, l, r, b, sel, cx)?)
        }
        Expr::Binary(op, l, r) => {
            let (x, y) = (eval(l, b, sel, cx)?, eval(r, b, sel, cx)?);
            Rc::new(binary(*op, &x, &y, b.len, sel)?)
        }
        Expr::Unary(op, x) => {
            let x = eval(x, b, sel, cx)?;
            Rc::new(unary(*op, &x, b.len, sel)?)
        }
        Expr::IsNull(x, negated) => {
            let x = eval(x, b, sel, cx)?;
            let mut out = vec![false; b.len];
            for i in sel.iter() {
                out[i] = x.is_null(i) != *negated;
            }
            Rc::new(Vector::Bool(out, None))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive: false,
        } if matches!(&**pattern, Expr::Literal(Value::Text(_))) => {
            let x = eval(expr, b, sel, cx)?;
            let Expr::Literal(Value::Text(p)) = &**pattern else {
                unreachable!("matched above")
            };
            match (&*x, like_shape(p)) {
                (Vector::Text(o, bytes, n), Some((needle, anchor))) => {
                    let mut out = vec![false; b.len];
                    match anchor {
                        None => kernels::eq_bytes(o, bytes, needle.as_bytes(), &mut out),
                        Some(a) => kernels::like_bytes(o, bytes, needle.as_bytes(), a, &mut out),
                    }
                    if *negated {
                        out.iter_mut().for_each(|v| *v = !*v);
                    }
                    Rc::new(Vector::Bool(out, n.clone()))
                }
                _ => {
                    let mut row = Vec::new();
                    each(b.len, sel, |i| {
                        row.clear();
                        row.extend(b.cols.iter().map(|c| c.value(i)));
                        e.eval(&cx.env(&row))
                    })?
                }
            }
        }
        Expr::Cast(x, ty) => {
            let x = eval(x, b, sel, cx)?;
            each(b.len, sel, |i| {
                crate::enums::cast(cx.ctx.txn, x.value(i), *ty)
            })?
        }
        Expr::Call(f, args) => {
            let args = args
                .iter()
                .map(|a| eval(a, b, sel, cx))
                .collect::<Result<Vec<_>>>()?;
            let call = CallContext::new(cx.ctx.txn, cx.ctx.registry);
            let mut vals = Vec::with_capacity(args.len());
            each(b.len, sel, |i| {
                vals.clear();
                vals.extend(args.iter().map(|a| a.value(i)));
                if f.strict() && vals.iter().any(Value::is_null) {
                    Ok(Value::Null)
                } else {
                    f.call(&call, &vals)
                }
            })?
        }
        // The rest a row at a time.
        _ => {
            let mut row = Vec::new();
            each(b.len, sel, |i| {
                row.clear();
                row.extend(b.cols.iter().map(|c| c.value(i)));
                e.eval(&cx.env(&row))
            })?
        }
    })
}

/// A `LIKE` pattern a kernel matches: the text, and where (`None`: the
/// whole string) — for patterns with `%` only at their ends, no `_` and no
/// escapes.
fn like_shape(p: &str) -> Option<(&str, Option<kernels::Anchor>)> {
    if p.contains(['_', '\\']) {
        return None;
    }
    let lead = p.starts_with('%');
    let trail = p.len() > 1 && p.ends_with('%') || (p == "%");
    let inner = p.trim_matches('%');
    if inner.contains('%') {
        return None;
    }
    Some((
        inner,
        match (lead, trail) {
            (false, false) => None,
            (false, true) => Some(kernels::Anchor::Prefix),
            (true, false) => Some(kernels::Anchor::Suffix),
            (true, true) => Some(kernels::Anchor::Contains),
        },
    ))
}

/// A vector of `f(i)` over the rows `sel` (typed when it can be).
fn each(len: usize, sel: &Sel, mut f: impl FnMut(usize) -> Result<Value>) -> Result<Rc<Vector>> {
    let mut out = vec![Value::Null; len];
    for i in sel.iter() {
        out[i] = f(i)?;
    }
    Ok(Rc::new(Vector::typed(out)))
}

/// Three-valued truth per row: 0 false, 1 true, 2 `NULL`.
fn truths(v: &Vector, len: usize, sel: &Sel) -> Result<Vec<u8>> {
    let mut out = vec![2u8; len];
    match v {
        Vector::Bool(d, n) => {
            for i in sel.iter() {
                if !is_null_at(n, i) {
                    out[i] = u8::from(d[i]);
                }
            }
        }
        _ => {
            for i in sel.iter() {
                out[i] = match v.value(i).truth()? {
                    None => 2,
                    Some(t) => u8::from(t),
                };
            }
        }
    }
    Ok(out)
}

/// `l AND r` / `l OR r`, the right side evaluated only where the left one
/// does not decide (as a row does).
fn logic(and: bool, l: &Expr, r: &Expr, b: &Batch, sel: &Sel, cx: Cx) -> Result<Vector> {
    let decides = u8::from(!and); // AND: false decides; OR: true
    let x = truths(&*eval(l, b, sel, cx)?, b.len, sel)?;
    let rest = sel.keep(|i| x[i] != decides);
    let y = truths(&*eval(r, b, &rest, cx)?, b.len, &rest)?;
    let mut data = vec![false; b.len];
    let mut nulls = vec![false; b.len];
    for i in sel.iter() {
        let t = if x[i] == decides || y[i] == decides {
            decides
        } else if x[i] == 2 || y[i] == 2 {
            2
        } else {
            1 - decides
        };
        data[i] = t == 1;
        nulls[i] = t == 2;
    }
    Ok(Vector::Bool(data, nulls.contains(&true).then_some(nulls)))
}

fn is_cmp(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq)
}

/// The kernels' comparison for `op`.
fn cmp_of(op: BinaryOp) -> Cmp {
    match op {
        BinaryOp::Eq => Cmp::Eq,
        BinaryOp::NotEq => Cmp::Ne,
        BinaryOp::Lt => Cmp::Lt,
        BinaryOp::LtEq => Cmp::Le,
        BinaryOp::Gt => Cmp::Gt,
        _ => Cmp::Ge,
    }
}

/// `x op y` as `y op' x`.
fn flipped(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        op => op,
    }
}

/// The rows where either side is `NULL`.
fn either_null(a: Option<&[bool]>, b: Option<&[bool]>, len: usize) -> Option<Vec<bool>> {
    match (a, b) {
        (None, None) => None,
        (Some(n), None) | (None, Some(n)) => Some(n[..len].to_vec()),
        (Some(a), Some(b)) => nulls_from(a.iter().zip(b).map(|(x, y)| x | y).collect()),
    }
}

/// An integer operand: its column (a constant spread over `len`) and its
/// `NULL`s; `None` for anything else.
fn int_col(v: &Vector, len: usize) -> Option<(std::borrow::Cow<'_, [i64]>, Option<&[bool]>)> {
    match v {
        Vector::Int(d, n) => Some((d[..len].into(), n.as_deref())),
        Vector::Const(Value::Int(i)) => Some((vec![*i; len].into(), None)),
        _ => None,
    }
}

fn int_rhs(v: &Vector) -> Option<(Rhs<'_, i64>, Option<&[bool]>)> {
    match v {
        Vector::Int(d, n) => Some((Rhs::Col(d), n.as_deref())),
        Vector::Const(Value::Int(i)) => Some((Rhs::Const(*i), None)),
        _ => None,
    }
}

/// A double operand (an integer widened, as a row widens it against a
/// double).
fn float_col(v: &Vector, len: usize) -> Option<(std::borrow::Cow<'_, [f64]>, Option<&[bool]>)> {
    match v {
        Vector::Float(d, n) => Some((d[..len].into(), n.as_deref())),
        Vector::Int(d, n) => Some((d[..len].iter().map(|&i| i as f64).collect(), n.as_deref())),
        Vector::Const(Value::Float(f)) => Some((vec![*f; len].into(), None)),
        Vector::Const(Value::Int(i)) => Some((vec![*i as f64; len].into(), None)),
        _ => None,
    }
}

/// `x op y` over the rows `sel`.
fn binary(op: BinaryOp, x: &Vector, y: &Vector, len: usize, sel: &Sel) -> Result<Vector> {
    use BinaryOp::*;
    let arith = matches!(op, Add | Sub | Mul | Div | Mod);
    if arith || is_cmp(op) {
        if let (Some(a), Some(b)) = (int_col(x, len), int_rhs(y)) {
            return int_kernel(op, a, b, len, sel);
        }
        let float_like =
            |v: &Vector| matches!(v, Vector::Float(..) | Vector::Const(Value::Float(_)));
        if (float_like(x) || float_like(y))
            && let (Some(a), Some(b)) = (float_col(x, len), float_col(y, len))
        {
            return float_kernel(op, a, b, len, sel);
        }
    }
    if is_cmp(op) {
        if let Some(v) = cmp_text(op, x, y, len)? {
            return Ok(v);
        }
        if let Some(v) = cmp_text(flipped(op), y, x, len)? {
            return Ok(v);
        }
        if let Some(v) = cmp_time(op, x, y, len) {
            return Ok(v);
        }
        if let Some(v) = cmp_time(flipped(op), y, x, len) {
            return Ok(v);
        }
    }
    let out = each(len, sel, |i| {
        crate::expr::binary(op, x.value(i), y.value(i))
    })?;
    Ok(Rc::try_unwrap(out).unwrap_or_else(|v| (*v).clone()))
}

fn int_kernel(
    op: BinaryOp,
    (a, an): (std::borrow::Cow<[i64]>, Option<&[bool]>),
    (b, bn): (Rhs<i64>, Option<&[bool]>),
    len: usize,
    sel: &Sel,
) -> Result<Vector> {
    use BinaryOp::*;
    let nulls = either_null(an, bn, len);
    let null = |i: usize| nulls.as_ref().is_some_and(|n| n[i]);
    if is_cmp(op) {
        let mut out = vec![false; len];
        kernels::cmp_i64(cmp_of(op), &a, b, &mut out);
        return Ok(Vector::Bool(out, nulls));
    }
    let mut out = vec![0i64; len];
    let over = |_| Error::out_of_range("bigint out of range");
    match op {
        Add | Sub | Mul => {
            let k = match op {
                Add => Arith::Add,
                Sub => Arith::Sub,
                _ => Arith::Mul,
            };
            let mut overflow = vec![false; len];
            kernels::arith_i64(k, &a, b, &mut out, &mut overflow);
            if let Some(i) = sel.iter().find(|&i| overflow[i] && !null(i)) {
                return Err(over(i));
            }
        }
        _ => {
            for i in sel.iter().filter(|&i| !null(i)) {
                let (x, y) = (
                    a[i],
                    match b {
                        Rhs::Col(b) => b[i],
                        Rhs::Const(c) => c,
                    },
                );
                if y == 0 {
                    return Err(Error::division_by_zero("division by zero"));
                }
                out[i] = match op {
                    Div => x.checked_div(y).ok_or_else(|| over(i))?,
                    _ => x.checked_rem(y).unwrap_or(0),
                };
            }
        }
    }
    Ok(Vector::Int(out, nulls))
}

fn float_kernel(
    op: BinaryOp,
    (a, an): (std::borrow::Cow<[f64]>, Option<&[bool]>),
    (b, bn): (std::borrow::Cow<[f64]>, Option<&[bool]>),
    len: usize,
    sel: &Sel,
) -> Result<Vector> {
    use BinaryOp::*;
    let nulls = either_null(an, bn, len);
    let null = |i: usize| nulls.as_ref().is_some_and(|n| n[i]);
    if is_cmp(op) {
        let mut out = vec![false; len];
        kernels::cmp_f64(cmp_of(op), &a, Rhs::Col(&b), &mut out);
        return Ok(Vector::Bool(out, nulls));
    }
    let mut out = vec![0f64; len];
    match op {
        Add => kernels::arith_f64(FArith::Add, &a, Rhs::Col(&b), &mut out),
        Sub => kernels::arith_f64(FArith::Sub, &a, Rhs::Col(&b), &mut out),
        Mul => kernels::arith_f64(FArith::Mul, &a, Rhs::Col(&b), &mut out),
        Div => kernels::arith_f64(FArith::Div, &a, Rhs::Col(&b), &mut out),
        _ => {
            for i in sel.iter() {
                out[i] = a[i] % b[i];
            }
        }
    }
    // What a row would refuse, on the rows in play.
    for i in sel.iter().filter(|&i| !null(i)) {
        let (x, y, r) = (a[i], b[i], out[i]);
        if matches!(op, Div | Mod) && y == 0.0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        crate::expr::check_float(
            op,
            r.is_infinite(),
            r == 0.0,
            [x.is_infinite(), y.is_infinite()],
            [x == 0.0, y == 0.0],
        )?;
    }
    Ok(Vector::Float(out, nulls))
}

/// `x op y` for a text vector `x` and text `y` (a vector or a constant).
fn cmp_text(op: BinaryOp, x: &Vector, y: &Vector, len: usize) -> Result<Option<Vector>> {
    let Vector::Text(o, b, xn) = x else {
        return Ok(None);
    };
    let at = |o: &[u32], i: usize| o[i] as usize..o[i + 1] as usize;
    let mut out = vec![false; len];
    let nulls = match y {
        Vector::Const(Value::Text(c)) => {
            if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                kernels::eq_bytes(o, b, c.as_bytes(), &mut out);
                if op == BinaryOp::NotEq {
                    out.iter_mut().for_each(|v| *v = !*v);
                }
            } else {
                let k = cmp_of(op);
                for (i, v) in out.iter_mut().enumerate() {
                    *v = cmp_holds(k, b[at(o, i)].cmp(c.as_bytes()));
                }
            }
            xn.clone()
        }
        Vector::Text(p, c, yn) => {
            let k = cmp_of(op);
            for (i, v) in out.iter_mut().enumerate() {
                *v = cmp_holds(k, b[at(o, i)].cmp(&c[at(p, i)]));
            }
            either_null(xn.as_deref(), yn.as_deref(), len)
        }
        _ => return Ok(None),
    };
    Ok(Some(Vector::Bool(out, nulls)))
}

/// `x op y` for a date / time vector `x` and a value of its kind.
fn cmp_time(op: BinaryOp, x: &Vector, y: &Vector, len: usize) -> Option<Vector> {
    let Vector::Time(kind, d, xn) = x else {
        return None;
    };
    let (rhs, yn) = match y {
        Vector::Time(k, e, yn) if k == kind => (Rhs::Col(&e[..]), yn.as_deref()),
        Vector::Const(v) => match TimeKind::of(v) {
            Some((k, c)) if k == *kind => (Rhs::Const(c), None),
            _ => return None,
        },
        _ => return None,
    };
    let mut out = vec![false; len];
    kernels::cmp_i64(cmp_of(op), &d[..len], rhs, &mut out);
    Some(Vector::Bool(out, either_null(xn.as_deref(), yn, len)))
}

fn cmp_holds(k: Cmp, o: std::cmp::Ordering) -> bool {
    match k {
        Cmp::Eq => o.is_eq(),
        Cmp::Ne => o.is_ne(),
        Cmp::Lt => o.is_lt(),
        Cmp::Le => o.is_le(),
        Cmp::Gt => o.is_gt(),
        Cmp::Ge => o.is_ge(),
    }
}

/// `op x` over the rows `sel`.
fn unary(op: UnaryOp, x: &Vector, len: usize, sel: &Sel) -> Result<Vector> {
    match (op, x) {
        (UnaryOp::Not, Vector::Bool(d, n)) => {
            let mut out = vec![false; len];
            for i in sel.iter() {
                out[i] = !d[i];
            }
            Ok(Vector::Bool(out, n.clone()))
        }
        (UnaryOp::Neg, Vector::Int(d, n)) => {
            let mut out = vec![0i64; len];
            for i in sel.iter() {
                if !is_null_at(n, i) {
                    out[i] = d[i]
                        .checked_neg()
                        .ok_or_else(|| Error::out_of_range("bigint out of range"))?;
                }
            }
            Ok(Vector::Int(out, n.clone()))
        }
        _ => {
            let out = each(len, sel, |i| crate::expr::unary(op, x.value(i)))?;
            Ok(Rc::try_unwrap(out).unwrap_or_else(|v| (*v).clone()))
        }
    }
}

/// The rows of `b` for which `p` holds.
fn filter(p: &Expr, b: Batch, cx: Cx) -> Result<Batch> {
    let v = eval(p, &b, &b.sel, cx)?;
    if let (Vector::Bool(d, n), Sel::All(len)) = (&*v, &b.sel) {
        let mut rows = Vec::new();
        match n {
            None => kernels::select(&d[..*len], &mut rows),
            Some(n) => {
                let mask: Vec<bool> = d.iter().zip(n).map(|(&x, &null)| x & !null).collect();
                kernels::select(&mask[..*len], &mut rows);
            }
        }
        return Ok(Batch {
            sel: Sel::Rows(rows),
            ..b
        });
    }
    let t = truths(&v, b.len, &b.sel)?;
    let sel = b.sel.keep(|i| t[i] == 1);
    Ok(Batch { sel, ..b })
}

/// `exprs` over the rows of `b`.
fn project(exprs: &[Expr], b: Batch, cx: Cx) -> Result<Batch> {
    let cols = exprs
        .iter()
        .map(|e| eval(e, &b, &b.sel, cx))
        .collect::<Result<_>>()?;
    Ok(Batch {
        len: b.len,
        cols,
        sel: b.sel,
    })
}

// ------------------------------------------------------------------ scans

/// How a column is decoded into a vector.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Skip,
    Int,
    Float,
    Bool,
    Text,
    Time(TimeKind),
    Values,
    Enum,
}

/// A vector being built.
enum Builder {
    Skip,
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Text(Vec<u32>, Vec<u8>, Vec<bool>),
    Time(TimeKind, Vec<i64>, Vec<bool>),
    Values(Vec<Value>),
}

impl Builder {
    fn new(kind: Kind, cap: usize) -> Self {
        let nulls = || Vec::with_capacity(cap);
        match kind {
            Kind::Skip => Builder::Skip,
            Kind::Int => Builder::Int(Vec::with_capacity(cap), nulls()),
            Kind::Float => Builder::Float(Vec::with_capacity(cap), nulls()),
            Kind::Bool => Builder::Bool(Vec::with_capacity(cap), nulls()),
            Kind::Text => {
                let mut offsets = Vec::with_capacity(cap + 1);
                offsets.push(0);
                Builder::Text(offsets, Vec::with_capacity(cap * 16), nulls())
            }
            Kind::Time(k) => Builder::Time(k, Vec::with_capacity(cap), nulls()),
            Kind::Values | Kind::Enum => Builder::Values(Vec::with_capacity(cap)),
        }
    }

    fn push_null(&mut self) {
        match self {
            Builder::Skip => {}
            Builder::Int(d, n) | Builder::Time(_, d, n) => {
                d.push(0);
                n.push(true);
            }
            Builder::Float(d, n) => {
                d.push(0.0);
                n.push(true);
            }
            Builder::Bool(d, n) => {
                d.push(false);
                n.push(true);
            }
            Builder::Text(o, b, n) => {
                o.push(b.len() as u32);
                n.push(true);
            }
            Builder::Values(v) => v.push(Value::Null),
        }
    }

    fn push_text(o: &mut Vec<u32>, b: &mut Vec<u8>, n: &mut Vec<bool>, s: &[u8]) {
        b.extend_from_slice(s);
        o.push(b.len() as u32);
        n.push(false);
    }

    fn push(&mut self, v: Value) {
        match (&mut *self, v) {
            (Builder::Skip, _) => {}
            (_, Value::Null) => self.push_null(),
            (Builder::Int(d, n), Value::Int(i)) => {
                d.push(i);
                n.push(false);
            }
            (Builder::Float(d, n), Value::Float(f)) => {
                d.push(f);
                n.push(false);
            }
            (Builder::Bool(d, n), Value::Bool(b)) => {
                d.push(b);
                n.push(false);
            }
            (Builder::Text(o, b, n), Value::Text(s)) => Self::push_text(o, b, n, s.as_bytes()),
            (Builder::Time(k, d, n), v) if TimeKind::of(&v).is_some_and(|(kv, _)| kv == *k) => {
                d.push(TimeKind::of(&v).expect("a time").1);
                n.push(false);
            }
            (Builder::Values(vals), v) => vals.push(v),
            (_, v) => {
                // Not the column's usual kind: values from here on.
                let mut vals = std::mem::replace(self, Builder::Skip).into_values();
                vals.push(v);
                *self = Builder::Values(vals);
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Builder::Skip => 0,
            Builder::Int(_, n)
            | Builder::Float(_, n)
            | Builder::Bool(_, n)
            | Builder::Text(_, _, n)
            | Builder::Time(_, _, n) => n.len(),
            Builder::Values(v) => v.len(),
        }
    }

    fn into_values(self) -> Vec<Value> {
        let n = self.len();
        match self.finish() {
            Vector::Values(v) => v,
            v => (0..n).map(|i| v.value(i)).collect(),
        }
    }

    fn finish(self) -> Vector {
        match self {
            Builder::Skip => Vector::Const(Value::Null),
            Builder::Int(d, n) => Vector::Int(d, nulls_from(n)),
            Builder::Float(d, n) => Vector::Float(d, nulls_from(n)),
            Builder::Bool(d, n) => Vector::Bool(d, nulls_from(n)),
            Builder::Text(o, b, n) => Vector::Text(o, b, nulls_from(n)),
            Builder::Time(k, d, n) => Vector::Time(k, d, nulls_from(n)),
            Builder::Values(v) => Vector::Values(v),
        }
    }
}

/// A primary key range: from, to (`None`: the index's end).
type KeyRange = (Vec<u8>, Option<Vec<u8>>, Option<usize>);

/// The row storage's rows a batch at a time, decoded from their encoding
/// straight into vectors ([`crate::storage::RowStorage::batches`]).
pub struct RowBatches<'a> {
    table: &'a Table,
    entries: Entries<'a>,
    kinds: Vec<Kind>,
    labels: Labels,
    txn: &'a dyn nucleus_kv::KvTxn,
}

impl<'a> RowBatches<'a> {
    /// Over `scan`'s range.
    pub fn new(scan: crate::storage::Scan<'a>) -> Result<Self> {
        let end = scan
            .end
            .as_deref()
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let entries = Entries::over(
            scan.txn,
            scan.validated,
            std::ops::Bound::Included(&scan.start),
            end,
        )?;
        let wanted = scan.wanted;
        let kinds = scan
            .table
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| {
                if !wanted.is_none_or(|w| w.get(c).copied().unwrap_or(false)) {
                    return Kind::Skip;
                }
                match col.ty {
                    DataType::SmallInt | DataType::Integer | DataType::BigInt => Kind::Int,
                    DataType::Float => Kind::Float,
                    DataType::Bool => Kind::Bool,
                    DataType::Text => Kind::Text,
                    DataType::Date => Kind::Time(TimeKind::Date),
                    DataType::Time => Kind::Time(TimeKind::Time),
                    DataType::Timestamp => Kind::Time(TimeKind::Timestamp),
                    DataType::TimestampTz => Kind::Time(TimeKind::TimestampTz),
                    DataType::Enum(_) => Kind::Enum,
                    _ => Kind::Values,
                }
            })
            .collect();
        Ok(Self {
            table: scan.table,
            entries,
            kinds,
            labels: Labels::default(),
            txn: scan.txn,
        })
    }
}

impl crate::storage::BatchSource for RowBatches<'_> {
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>> {
        let mut cols: Vec<Builder> = self.kinds.iter().map(|&k| Builder::new(k, max)).collect();
        let mut n = 0;
        while n < max {
            let Some((key, bytes)) = self.entries.next()? else {
                break;
            };
            decode_into(self.txn, self.table, key, bytes, &mut cols)?;
            n += 1;
        }
        if n == 0 {
            return Ok(None);
        }
        let mut out: Vec<Rc<Vector>> = Vec::with_capacity(cols.len());
        for (c, b) in cols.into_iter().enumerate() {
            let mut v = b.finish();
            if self.kinds[c] == Kind::Enum
                && let Vector::Values(vals) = &mut v
            {
                for x in vals {
                    self.labels.fill(self.txn, self.table.columns[c].ty, x)?;
                }
            }
            out.push(Rc::new(v));
        }
        Ok(Some(Batch {
            len: n,
            cols: out,
            sel: Sel::All(n),
        }))
    }
}

/// Rows as a batch (a vector per column, typed where the values are);
/// `None` for no rows.
pub fn batch_of_rows(rows: Vec<Row>) -> Option<Batch> {
    let n = rows.len();
    let width = rows.first()?.len();
    let mut cols: Vec<Vec<Value>> = (0..width).map(|_| Vec::with_capacity(n)).collect();
    for row in rows {
        for (c, v) in row.into_iter().enumerate() {
            if let Some(col) = cols.get_mut(c) {
                col.push(v);
            }
        }
    }
    Some(Batch {
        len: n,
        cols: cols
            .into_iter()
            .map(|c| Rc::new(Vector::typed(c)))
            .collect(),
        sel: Sel::All(n),
    })
}

/// Appends a stored row's columns to `cols` (a value stored out of line
/// fetched only for a column read).
fn decode_into(
    txn: &dyn nucleus_kv::KvTxn,
    t: &Table,
    key: &[u8],
    b: &[u8],
    cols: &mut [Builder],
) -> Result<()> {
    let bad = |_| Error::internal("malformed row encoding");
    let mut r = Reader::new(b);
    if !(1..=crate::encoding::ROW_VERSION).contains(&r.u8().map_err(bad)?) {
        return Err(Error::internal("malformed row encoding"));
    }
    let n = r.u16().map_err(bad)? as usize;
    for c in 0..n {
        let Some(col) = cols.get_mut(c) else {
            break; // stored past the table's columns: not read
        };
        let tag = r.peek();
        if tag == Some(OUT_OF_LINE) {
            r.u8().map_err(bad)?;
            let len = r.u32().map_err(bad)?;
            if !matches!(col, Builder::Skip) {
                let pk = crate::exec::pk_suffix(t, key);
                col.push(crate::large::fetch(txn, t, &pk, c, len)?);
            }
            continue;
        }
        match (col, tag) {
            (Builder::Skip, _) => crate::encoding::skip_value(&mut r)?,
            (col, Some(0)) => {
                r.u8().map_err(bad)?;
                col.push_null();
            }
            (Builder::Int(d, nulls), Some(12)) => {
                r.u8().map_err(bad)?;
                d.push(i64::from(r.u16().map_err(bad)? as i16));
                nulls.push(false);
            }
            (Builder::Int(d, nulls), Some(13)) => {
                r.u8().map_err(bad)?;
                d.push(i64::from(r.u32().map_err(bad)? as i32));
                nulls.push(false);
            }
            (Builder::Int(d, nulls), Some(2)) => {
                r.u8().map_err(bad)?;
                d.push(r.u64().map_err(bad)? as i64);
                nulls.push(false);
            }
            (Builder::Float(d, nulls), Some(3)) => {
                r.u8().map_err(bad)?;
                d.push(f64::from_bits(r.u64().map_err(bad)?));
                nulls.push(false);
            }
            (Builder::Text(o, bytes, nulls), Some(4)) => {
                r.u8().map_err(bad)?;
                let len = r.u32().map_err(bad)? as usize;
                Builder::push_text(o, bytes, nulls, r.take(len).map_err(bad)?);
            }
            (Builder::Time(k, d, nulls), Some(tag @ (6 | 7 | 8 | 17)))
                if matches!(
                    (*k, tag),
                    (TimeKind::Date, 6)
                        | (TimeKind::Timestamp, 7)
                        | (TimeKind::TimestampTz, 8)
                        | (TimeKind::Time, 17)
                ) =>
            {
                r.u8().map_err(bad)?;
                d.push(if tag == 6 {
                    i64::from(r.u32().map_err(bad)? as i32)
                } else {
                    r.u64().map_err(bad)? as i64
                });
                nulls.push(false);
            }
            (col, _) => col.push(crate::encoding::decode_value(&mut r)?),
        }
    }
    for col in cols.iter_mut().skip(n) {
        col.push_null();
    }
    Ok(())
}

// --------------------------------------------------------------- pipelines

/// A step of a pipeline above its scan.
enum Step<'a> {
    Filter(&'a Expr),
    Project(&'a [Expr]),
}

/// What a pipeline produced.
enum Out {
    /// A batch.
    Batch(Batch),
    /// A batch whose evaluation failed, run again a row at a time: the rows
    /// it produced, and the error it stopped at (`None`: it did not fail
    /// a row at a time).
    Rows(Vec<Row>, Option<Error>),
}

/// A table scan and the filters and projections above it, run a batch at
/// a time.
pub struct Pipeline<'a> {
    /// The scan's batches (`None`: an empty key range).
    source: Option<Box<dyn crate::storage::BatchSource + 'a>>,
    /// The scan's node.
    scan: &'a Plan,
    /// The steps, bottom up, each with its plan node.
    steps: Vec<(Step<'a>, &'a Plan)>,
    /// The node the pipeline was built for (measured by its caller).
    top: &'a Plan,
    cx: Cx<'a>,
    batch: usize,
    max: usize,
}

/// The scan, and the filters and projections above it, of a chain that
/// ends in a table scan by the primary key (or every row); `None` for
/// anything else.
fn chain(plan: &Plan) -> Option<(&Plan, Vec<(Step<'_>, &Plan)>)> {
    match plan {
        Plan::Scan { access, filter, .. } => {
            if matches!(access, Access::Key { index: Some(_), .. }) {
                return None;
            }
            let steps = filter.iter().map(|f| (Step::Filter(f), plan)).collect();
            Some((plan, steps))
        }
        Plan::Filter { input, predicate }
        | Plan::Secure {
            input,
            policy: predicate,
        } => {
            let (scan, mut steps) = chain(input)?;
            steps.push((Step::Filter(predicate), plan));
            Some((scan, steps))
        }
        Plan::Project { input, exprs } => {
            let (scan, mut steps) = chain(input)?;
            steps.push((Step::Project(exprs), plan));
            Some((scan, steps))
        }
        _ => None,
    }
}

/// The column ranges the filters right above a scan imply (before any
/// projection): conjuncts `column op constant`.
fn scan_bounds(steps: &[(Step, &Plan)], ctx: &Ctx) -> Vec<crate::storage::ColumnBound> {
    let mut out = Vec::new();
    for (step, _) in steps {
        match step {
            Step::Filter(p) => bounds_of(p, ctx, &mut out),
            Step::Project(_) => break,
        }
    }
    out
}

fn bounds_of(e: &Expr, ctx: &Ctx, out: &mut Vec<crate::storage::ColumnBound>) {
    use std::ops::Bound::*;
    let constant = |e: &Expr| match e {
        Expr::Literal(v) => Some(v.clone()),
        Expr::Param(i) => ctx.params.get(*i).cloned(),
        _ => None,
    };
    match e {
        Expr::Binary(BinaryOp::And, l, r) => {
            bounds_of(l, ctx, out);
            bounds_of(r, ctx, out);
        }
        Expr::Binary(op, l, r) => {
            let (col, v, op) = match (&**l, &**r) {
                (Expr::Column(c), x) => match constant(x) {
                    Some(v) => (*c, v, *op),
                    None => return,
                },
                (x, Expr::Column(c)) => match constant(x) {
                    Some(v) => (*c, v, flipped(*op)),
                    None => return,
                },
                _ => return,
            };
            if v.is_null() {
                return;
            }
            let (lower, upper) = match op {
                BinaryOp::Eq => (Included(v.clone()), Included(v)),
                BinaryOp::Lt => (Unbounded, Excluded(v)),
                BinaryOp::LtEq => (Unbounded, Included(v)),
                BinaryOp::Gt => (Excluded(v), Unbounded),
                BinaryOp::GtEq => (Included(v), Unbounded),
                _ => return,
            };
            out.push(crate::storage::ColumnBound {
                column: col,
                lower,
                upper,
            });
        }
        _ => {}
    }
}

impl<'a> Pipeline<'a> {
    /// A pipeline for `plan` — when it is a chain that can run so, and
    /// vectorized execution is on.  `rows`: whether its output becomes
    /// rows (so a chain without a filter gains nothing).
    fn new(
        plan: &'a Plan,
        top: &'a Plan,
        ctx: &'a Ctx<'a>,
        outer: Option<&'a Env<'a>>,
        rows: bool,
        range: Option<KeyRange>,
    ) -> Result<Option<Self>> {
        let max = crate::budget::batch_size();
        if max == 0 {
            return Ok(None);
        }
        let Some((scan, steps)) = chain(plan) else {
            return Ok(None);
        };
        if rows && !steps.iter().any(|(s, _)| matches!(s, Step::Filter(_))) {
            return Ok(None);
        }
        let Plan::Scan { table, access, .. } = scan else {
            unreachable!("a chain ends in a scan")
        };
        let cx = Cx { ctx, outer };
        let wanted = ctx.needed.get(&(scan as *const Plan as usize));
        let range = match range {
            Some(r) => Some(r),
            None => crate::exec::key_range(table, access, &cx.env(&[]))?,
        };
        let source = match range {
            None => None,
            Some((start, end, _)) => {
                Some(crate::storage::of(table)?.batches(crate::storage::Scan {
                    txn: ctx.txn,
                    table,
                    start,
                    end,
                    wanted: wanted.map(Vec::as_slice),
                    validated: ctx.validated,
                    bounds: scan_bounds(&steps, ctx),
                })?)
            }
        };
        if let Some(p) = ctx.profile {
            let mut nodes: Vec<&Plan> = vec![scan];
            for (_, n) in &steps {
                if !nodes.iter().any(|m| std::ptr::eq(*m, *n)) {
                    nodes.push(n);
                }
            }
            for node in nodes {
                if !std::ptr::eq(node, top) {
                    p.record(node, Duration::ZERO, 0, 1);
                }
            }
        }
        Ok(Some(Self {
            source,
            scan,
            steps,
            top,
            cx,
            batch: FIRST_BATCH.min(max),
            max,
        }))
    }

    fn next(&mut self) -> Result<Option<Out>> {
        loop {
            let started = self.cx.ctx.profile.and_then(|p| p.clock());
            let Some(source) = &mut self.source else {
                return Ok(None);
            };
            let Some(input) = source.next_batch(self.batch)? else {
                return Ok(None);
            };
            crate::budget::charge_scans(input.len as u64)?;
            self.batch = (self.batch * 2).min(self.max);
            let out = match self.run(&input, started) {
                Ok(b) if b.sel.is_empty() => continue,
                Ok(b) => Out::Batch(b),
                Err(_) => self.redo(&input),
            };
            return Ok(Some(out));
        }
    }

    /// The steps over `input`, each node's rows (and time, its inputs'
    /// included) recorded under `EXPLAIN ANALYZE`.
    fn run(&self, input: &Batch, started: Option<Duration>) -> Result<Batch> {
        let profile = self.cx.ctx.profile;
        let record = |node: &Plan, b: &Batch| {
            if let Some(p) = profile
                && !std::ptr::eq(node, self.top)
            {
                let spent = match (started, p.clock()) {
                    (Some(a), Some(b)) => b.saturating_sub(a),
                    _ => Duration::ZERO,
                };
                p.record(node, spent, b.sel.len() as u64, 0);
            }
        };
        let mut b = input.clone();
        let mut steps = self.steps.iter().peekable();
        if steps
            .peek()
            .is_none_or(|(_, n)| !std::ptr::eq(*n, self.scan))
        {
            record(self.scan, &b);
        }
        while let Some((step, node)) = steps.next() {
            b = match step {
                Step::Filter(p) => filter(p, b, self.cx)?,
                Step::Project(exprs) => project(exprs, b, self.cx)?,
            };
            if steps.peek().is_none_or(|(_, n)| !std::ptr::eq(*n, *node)) {
                record(node, &b);
            }
        }
        Ok(b)
    }

    /// `input` through the steps a row at a time (after an error on the
    /// batch: a row at a time decides).
    fn redo(&self, input: &Batch) -> Out {
        let mut rows = Vec::new();
        'rows: for i in input.sel.iter() {
            let mut row = input.row(i);
            for (step, _) in &self.steps {
                match step {
                    Step::Filter(p) => match p.holds(&self.cx.env(&row)) {
                        Ok(true) => {}
                        Ok(false) => continue 'rows,
                        Err(e) => return Out::Rows(rows, Some(e)),
                    },
                    Step::Project(exprs) => {
                        let e = self.cx.env(&row);
                        match exprs.iter().map(|x| x.eval(&e)).collect::<Result<Row>>() {
                            Ok(r) => row = r,
                            Err(e) => return Out::Rows(rows, Some(e)),
                        }
                    }
                }
            }
            rows.push(row);
        }
        Out::Rows(rows, None)
    }
}

/// `plan` run vectorized, as rows — when it can be (see [`Pipeline`]).
pub fn rows<'a>(
    plan: &'a Plan,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    let filters = chain(plan)
        .is_some_and(|(_, steps)| steps.iter().any(|(s, _)| matches!(s, Step::Filter(_))));
    if filters
        && crate::budget::batch_size() > 0
        && let Some(work) = Morsels::new(plan, &[], ctx, outer, ROW_MORSELS_PER_WORKER)?
    {
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Some(Box::new(ParallelRows {
            work,
            plan,
            next: 0,
            rows: std::collections::VecDeque::new(),
            err: None,
            done: false,
        })));
    }
    let Some(pipe) = Pipeline::new(plan, plan, ctx, outer, true, None)? else {
        return Ok(None);
    };
    Ok(Some(Box::new(BatchRows {
        pipe,
        batch: None,
        pos: 0,
        pending: std::collections::VecDeque::new(),
        err: None,
        done: false,
    })))
}

/// A result as batches ([`batches`]).
pub type Batches<'a> = Box<dyn Iterator<Item = Result<Batch>> + 'a>;

/// `plan` run vectorized, as batches — when it can be (see [`Pipeline`];
/// not row by row, `batch_size` 0): the rows, their order and the errors
/// of [`rows`], for a sink that takes columns (a columnar result) without
/// a value per cell.  A batch run again row by row comes as a batch of
/// those rows; the batches of a parallel run come in key order.
pub fn batches<'a>(
    plan: &'a Plan,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Batches<'a>>> {
    if crate::budget::batch_size() == 0 {
        return Ok(None);
    }
    let filters = chain(plan)
        .is_some_and(|(_, steps)| steps.iter().any(|(s, _)| matches!(s, Step::Filter(_))));
    if filters && let Some(work) = Morsels::new(plan, &[], ctx, outer, ROW_MORSELS_PER_WORKER)? {
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Some(Box::new(ParallelBatches {
            work,
            plan,
            next: 0,
            batches: std::collections::VecDeque::new(),
            err: None,
            done: false,
        })));
    }
    let Some(pipe) = Pipeline::new(plan, plan, ctx, outer, false, None)? else {
        return Ok(None);
    };
    Ok(Some(Box::new(PipeBatches {
        pipe,
        err: None,
        done: false,
    })))
}

/// A pipeline's batches, those run again row by row as batches of their
/// rows.
struct PipeBatches<'a> {
    pipe: Pipeline<'a>,
    err: Option<Error>,
    done: bool,
}

impl Iterator for PipeBatches<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Result<Batch>> {
        loop {
            if self.done {
                return None;
            }
            if let Some(e) = self.err.take() {
                self.done = true;
                return Some(Err(e));
            }
            match self.pipe.next() {
                Ok(Some(Out::Batch(b))) if b.sel.is_empty() => {}
                Ok(Some(Out::Batch(b))) => return Some(Ok(b)),
                Ok(Some(Out::Rows(rows, err))) => {
                    self.err = err;
                    if let Some(b) = batch_of_rows(rows) {
                        return Some(Ok(b));
                    }
                }
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// A pipeline's batches as rows.
struct BatchRows<'a> {
    pipe: Pipeline<'a>,
    batch: Option<Batch>,
    pos: usize,
    pending: std::collections::VecDeque<Row>,
    err: Option<Error>,
    done: bool,
}

impl Iterator for BatchRows<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if self.done {
                return None;
            }
            if let Some(r) = self.pending.pop_front() {
                return Some(Ok(r));
            }
            if let Some(e) = self.err.take() {
                self.done = true;
                return Some(Err(e));
            }
            if let Some(b) = &self.batch
                && self.pos < b.sel.len()
            {
                let i = b.sel.nth(self.pos);
                self.pos += 1;
                return Some(Ok(b.row(i)));
            }
            self.batch = None;
            match self.pipe.next() {
                Ok(Some(Out::Batch(b))) => {
                    self.batch = Some(b);
                    self.pos = 0;
                }
                Ok(Some(Out::Rows(rows, err))) => {
                    self.pending = rows.into();
                    self.err = err;
                }
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// -------------------------------------------------------------- aggregates

/// An aggregate without the batch capability, fed row by row.
struct PerGroup {
    agg: AggRef,
    accs: Vec<Box<dyn Accumulator>>,
}

impl GroupAccumulator for PerGroup {
    fn resize(&mut self, groups: usize) {
        while self.accs.len() < groups {
            self.accs.push(self.agg.start());
        }
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let mut vals = Vec::with_capacity(args.len());
        for (&r, &g) in rows.iter().zip(groups) {
            vals.clear();
            vals.extend(args.iter().map(|a| a.value(r as usize)));
            self.accs[g as usize].add(&vals)?;
        }
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.accs[group].finish()
    }
}

/// Group numbers by key.
enum Keys {
    /// No `GROUP BY`: one group.
    Total,
    /// One integer key (`NULL`'s group apart).
    Int(HashMap<i64, u32, MixState>, Option<u32>),
    /// Any keys.
    Any(HashMap<Vec<Value>, u32, MixState>),
}

/// A vectorized aggregate's state.
struct Grouping<'a> {
    group: &'a [Expr],
    aggs: &'a [Agg],
    keys: Keys,
    /// Each group's key, in the order the groups were first seen.
    seen: Vec<Vec<Value>>,
    accs: Vec<Box<dyn GroupAccumulator>>,
    bytes: usize,
    limit: usize,
    parts: Option<crate::spill::Partitions>,
}

impl<'a> Grouping<'a> {
    fn new(group: &'a [Expr], aggs: &'a [Agg]) -> Self {
        Grouping {
            group,
            aggs,
            keys: match group.len() {
                0 => Keys::Total,
                _ => Keys::Any(HashMap::default()),
            },
            seen: Vec::new(),
            accs: aggs
                .iter()
                .map(|a| {
                    a.func.grouped().unwrap_or_else(|| {
                        Box::new(PerGroup {
                            agg: a.func.clone(),
                            accs: Vec::new(),
                        })
                    })
                })
                .collect(),
            bytes: 0,
            limit: crate::budget::work_mem(),
            parts: None,
        }
    }

    /// The group of the row with key `key` (made if new), or `None` when
    /// the row goes to a partition (past work_mem, as a row would).
    fn group_of(&mut self, key: &dyn Fn() -> Vec<Value>, int: Option<Option<i64>>) -> Option<u32> {
        let found = match (&self.keys, int) {
            (Keys::Total, _) => self.seen.first().map(|_| 0),
            (Keys::Int(m, _), Some(Some(i))) => m.get(&i).copied(),
            (Keys::Int(_, null), Some(None)) => *null,
            (Keys::Any(m), _) => m.get(&key()).copied(),
            (Keys::Int(..), None) => unreachable!("an integer key"),
        };
        if found.is_some() {
            return found;
        }
        let key = key();
        let size = crate::spill::row_bytes(&key) + 96 * self.aggs.len();
        if self.parts.is_some()
            || (self.bytes + size > self.limit
                && crate::spill::MAX_LEVEL > 0
                && !self.group.is_empty())
        {
            return None;
        }
        self.bytes += size;
        let g = self.seen.len() as u32;
        match (&mut self.keys, int) {
            (Keys::Total, _) => {}
            (Keys::Int(m, _), Some(Some(i))) => {
                m.insert(i, g);
            }
            (Keys::Int(_, null), _) => *null = Some(g),
            (Keys::Any(m), _) => {
                m.insert(key.clone(), g);
            }
        }
        self.seen.push(key);
        for a in &mut self.accs {
            a.resize(self.seen.len());
        }
        Some(g)
    }

    /// Switches integer keys to any keys.
    fn widen(&mut self) {
        if let Keys::Int(..) = self.keys {
            let m = self
                .seen
                .iter()
                .enumerate()
                .map(|(g, k)| (k.clone(), g as u32))
                .collect();
            self.keys = Keys::Any(m);
        }
    }

    /// Adds the rows `sel` of a batch whose group keys and aggregate
    /// arguments are `keys` and `args`; `row` makes a row's input row (for
    /// a partition).
    fn add(
        &mut self,
        sel: &Sel,
        keys: &[Rc<Vector>],
        args: &[Vec<Rc<Vector>>],
        row: &dyn Fn(usize) -> Row,
    ) -> Result<()> {
        // A grand total over a whole batch: each aggregate fed at once.
        if let (Keys::Total, Sel::All(len)) = (&self.keys, sel) {
            let g = self
                .group_of(&Vec::new, None)
                .expect("a grand total stays in memory");
            for ((agg, acc), args) in self.aggs.iter().zip(&mut self.accs).zip(args) {
                let refs: Vec<&Vector> = args.iter().map(|a| &**a).collect();
                acc.update_all(*len, g, !agg.func.keeps_nulls(), &refs)?;
            }
            return Ok(());
        }
        // A single integer key: hashed as an integer.
        let int_key = match keys {
            [k] if matches!(**k, Vector::Int(..)) => {
                if self.seen.is_empty() && matches!(self.keys, Keys::Any(_)) {
                    self.keys = Keys::Int(HashMap::default(), None);
                }
                matches!(self.keys, Keys::Int(..))
            }
            _ => {
                self.widen();
                false
            }
        };
        let mut rows = Vec::with_capacity(sel.len());
        let mut groups = Vec::with_capacity(sel.len());
        for i in sel.iter() {
            let key = || keys.iter().map(|k| k.value(i)).collect::<Vec<_>>();
            let int = int_key.then(|| (!keys[0].is_null(i)).then(|| keys[0].value(i)));
            let int = int.map(|v| match v {
                Some(Value::Int(x)) => Some(x),
                _ => None,
            });
            match self.group_of(&key, int) {
                Some(g) => {
                    rows.push(i as u32);
                    groups.push(g);
                }
                None => self
                    .parts
                    .get_or_insert_with(|| crate::spill::Partitions::new(0))
                    .push(&key(), &row(i))?,
            }
        }
        for ((agg, acc), args) in self.aggs.iter().zip(&mut self.accs).zip(args) {
            let refs: Vec<&Vector> = args.iter().map(|a| &**a).collect();
            match refs.first() {
                Some(first) if !agg.func.keeps_nulls() => {
                    let (mut r, mut g) = (Vec::new(), Vec::new());
                    for (&row, &grp) in rows.iter().zip(&groups) {
                        if !first.is_null(row as usize) {
                            r.push(row);
                            g.push(grp);
                        }
                    }
                    acc.update(&r, &g, &refs)?;
                }
                _ => acc.update(&rows, &groups, &refs)?,
            }
        }
        Ok(())
    }

    /// Adds one row, evaluated a row at a time.
    fn add_row(&mut self, row: &Row, cx: Cx) -> Result<()> {
        let e = cx.env(row);
        let keys = self
            .group
            .iter()
            .map(|g| g.eval(&e).map(|v| Rc::new(Vector::Const(v))))
            .collect::<Result<Vec<_>>>()?;
        let args = self
            .aggs
            .iter()
            .map(|a| {
                a.args
                    .iter()
                    .map(|x| x.eval(&e).map(|v| Rc::new(Vector::Const(v))))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        self.widen();
        self.add(&Sel::All(1), &keys, &args, &|_| row.clone())
    }
}

/// Feeds a pipeline's rows to `g`.
fn feed(g: &mut Grouping, pipe: &mut Pipeline, cx: Cx) -> Result<()> {
    let (group, aggs) = (g.group, g.aggs);
    while let Some(out) = pipe.next()? {
        match out {
            Out::Batch(b) => {
                let eval_all = || -> Result<(Vec<Rc<Vector>>, Vec<Vec<Rc<Vector>>>)> {
                    let keys = group
                        .iter()
                        .map(|x| eval(x, &b, &b.sel, cx))
                        .collect::<Result<Vec<_>>>()?;
                    let args = aggs
                        .iter()
                        .map(|a| {
                            a.args
                                .iter()
                                .map(|x| eval(x, &b, &b.sel, cx))
                                .collect::<Result<Vec<_>>>()
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok((keys, args))
                };
                match eval_all() {
                    Ok((keys, args)) => g.add(&b.sel, &keys, &args, &|i| b.row(i))?,
                    // A row at a time decides.
                    Err(_) => {
                        for i in b.sel.iter() {
                            g.add_row(&b.row(i), cx)?;
                        }
                    }
                }
            }
            Out::Rows(rows, err) => {
                for row in &rows {
                    g.add_row(row, cx)?;
                }
                if let Some(e) = err {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// The groups' rows: key, then each aggregate's result.
fn finish(g: &mut Grouping) -> Result<Vec<Row>> {
    if g.seen.is_empty() && g.group.is_empty() {
        g.seen.push(Vec::new()); // aggregates over no rows
        for a in &mut g.accs {
            a.resize(1);
        }
    }
    let mut done = Vec::with_capacity(g.seen.len());
    for (i, key) in std::mem::take(&mut g.seen).into_iter().enumerate() {
        let mut row = key;
        for a in &mut g.accs {
            row.push(a.finish(i)?);
        }
        done.push(row);
    }
    Ok(done)
}

/// `GROUP BY group` with `aggs` over `input`, vectorized — when the input
/// is a pipeline and no aggregate has `DISTINCT` or `ORDER BY`.
pub fn aggregate<'a>(
    input: &'a Plan,
    top: &'a Plan,
    group: &'a [Expr],
    aggs: &'a [Agg],
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    if aggs.iter().any(|a| a.distinct || !a.order.is_empty()) {
        return Ok(None);
    }
    if let Some(rows) = parallel_aggregate(input, top, group, aggs, ctx, outer)? {
        return Ok(Some(rows));
    }
    let Some(mut pipe) = Pipeline::new(input, top, ctx, outer, false, None)? else {
        return Ok(None);
    };
    let cx = Cx { ctx, outer };
    let mut g = Grouping::new(group, aggs);
    feed(&mut g, &mut pipe, cx)?;
    let done = finish(&mut g)?;
    let runs = match g.parts.take() {
        Some(p) => p.seal()?,
        None => Vec::new(),
    };
    Ok(Some(Box::new(done.into_iter().map(Ok).chain(
        runs.into_iter().flat_map(move |run| {
            match crate::exec::aggregate(Box::new(run.iter()), group, aggs, ctx, outer, 1) {
                Ok(rows) => rows,
                Err(e) => Box::new(std::iter::once(Err(e))),
            }
        }),
    ))))
}

// ---------------------------------------------------------------- parallel

/// Morsels per worker: enough that one slow morsel does not keep the rest
/// waiting.
const MORSELS_PER_WORKER: usize = 4;

static PARALLEL_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many aggregates and pipelines this process has run in parallel
/// (for tests and diagnostics).
pub fn parallel_runs() -> u64 {
    PARALLEL_RUNS.load(std::sync::atomic::Ordering::Relaxed)
}

/// What one morsel's aggregation gives the merge.
enum Part {
    /// Its groups' keys, in the order first seen, and each aggregate's
    /// partial state of each group.
    Groups(Vec<Vec<Value>>, Vec<Vec<Value>>),
    /// Its groups outgrew work_mem.
    TooLarge,
}

/// Whether `e` gives the same values on any thread: no subqueries (they
/// would run once per worker), nothing volatile (whose values depend on
/// the order of calls).
pub(crate) fn parallel_safe(e: &Expr) -> bool {
    let mut safe = !e.has_subquery();
    e.walk(&mut |x| {
        if let Expr::Call(f, _) = x
            && f.volatility() == crate::ext::Volatility::Volatile
        {
            safe = false;
        }
    });
    safe
}

/// A chain's scan cut into morsels for worker threads, and what the
/// workers share.
struct Morsels<'a> {
    shared: &'a dyn nucleus_kv::KvRead,
    ctx: &'a Ctx<'a>,
    ranges: Vec<KeyRange>,
    workers: usize,
}

impl<'a> Morsels<'a> {
    /// The morsels of `input`'s scan — about `per_worker` a worker — when
    /// the chain and `exprs` can run in parallel: more than one worker, no
    /// enclosing row, no `EXPLAIN ANALYZE`, nothing that would differ by
    /// thread ([`parallel_safe`]), a transaction that shares its reads,
    /// and a range the KV splits.
    fn new(
        input: &'a Plan,
        exprs: &[&Expr],
        ctx: &'a Ctx<'a>,
        outer: Option<&'a Env<'a>>,
        per_worker: usize,
    ) -> Result<Option<Self>> {
        let workers = crate::budget::workers();
        if workers < 2 || outer.is_some() || ctx.profile.is_some() {
            return Ok(None);
        }
        let Some((scan, steps)) = chain(input) else {
            return Ok(None);
        };
        let Plan::Scan { table, access, .. } = scan else {
            unreachable!("a chain ends in a scan")
        };
        let mut all: Vec<&Expr> = exprs.to_vec();
        for (step, _) in &steps {
            match step {
                Step::Filter(p) => all.push(p),
                Step::Project(es) => all.extend(es.iter()),
            }
        }
        if !all.iter().all(|e| parallel_safe(e)) {
            return Ok(None);
        }
        let Some(shared) = ctx.txn.shared() else {
            return Ok(None);
        };
        let Some((start, end, _)) = crate::exec::key_range(table, access, &env(&[], ctx, outer))?
        else {
            return Ok(None);
        };
        let splits = crate::storage::of(table)?.split_points(
            ctx.txn,
            table,
            &start,
            end.as_deref(),
            workers * per_worker,
        )?;
        if splits.is_empty() {
            return Ok(None);
        }
        let mut bounds = vec![start];
        bounds.extend(splits);
        let n = bounds.len();
        let ranges = (0..n)
            .map(|i| {
                let to = if i + 1 < n {
                    Some(bounds[i + 1].clone())
                } else {
                    end.clone()
                };
                (bounds[i].clone(), to, None)
            })
            .collect();
        Ok(Some(Morsels {
            shared,
            ctx,
            ranges,
            workers,
        }))
    }

    /// `f` over the morsels `which`, on up to `workers` threads, each with
    /// a context of its own over the shared reads and the statement's
    /// ambient state; the results in morsel order.  `stop`: after a morsel
    /// fails, the later ones need not run (theirs are `None`).
    fn run<T: Send>(
        &self,
        which: std::ops::Range<usize>,
        stop: bool,
        f: &(dyn Fn(&Ctx, KeyRange) -> Result<T> + Sync),
    ) -> Vec<Option<Result<T>>> {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
        let ambient = crate::parallel::Ambient::capture();
        let ctx = self.ctx;
        let (registry, params, needed, validated, memoize) = (
            ctx.registry,
            ctx.params,
            &ctx.needed,
            ctx.validated,
            ctx.memoize,
        );
        let (shared, ranges, workers) = (self.shared, &self.ranges, self.workers);
        let first = which.start;
        let n = which.len();
        let next = AtomicUsize::new(0);
        let failed = AtomicUsize::new(usize::MAX);
        let scanned = AtomicU64::new(0);
        let results: Vec<Mutex<Option<Result<T>>>> = (0..n).map(|_| Mutex::new(None)).collect();
        std::thread::scope(|sc| {
            for _ in 0..workers.min(n) {
                sc.spawn(|| {
                    let ((), rows) = ambient.run(|| {
                        loop {
                            let m = next.fetch_add(1, SeqCst);
                            if m >= n || (stop && m > failed.load(SeqCst)) {
                                break;
                            }
                            let txn = crate::parallel::SharedTxn(shared);
                            let mut wctx = Ctx::new(&txn, registry, params);
                            wctx.needed = needed.clone();
                            wctx.validated = validated;
                            wctx.memoize = memoize;
                            let r = f(&wctx, ranges[first + m].clone());
                            if r.is_err() {
                                failed.fetch_min(m, SeqCst);
                            }
                            *results[m].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                        }
                    });
                    scanned.fetch_add(rows, SeqCst);
                });
            }
        });
        crate::budget::add_scanned(scanned.into_inner());
        results
            .into_iter()
            .map(|r| r.into_inner().unwrap_or_else(|e| e.into_inner()))
            .collect()
    }
}

/// `GROUP BY group` with `aggs` over the pipeline `input`, in parallel: the
/// scan's key range cut into morsels at the KV's split points, each
/// aggregated by a worker thread on its own, the morsels' partial states
/// combined in key order — so groups come out in the order a single pass
/// sees them, with the same results (sums of doubles are exact, so their
/// order does not matter).  `None` when it cannot run so: one worker, an
/// enclosing row, `EXPLAIN ANALYZE`, a subquery or volatile function, an
/// aggregate that cannot combine states, a transaction that cannot share
/// its reads, a range too small to split — or groups past work_mem (then
/// the single pass partitions them).
fn parallel_aggregate<'a>(
    input: &'a Plan,
    top: &'a Plan,
    group: &'a [Expr],
    aggs: &'a [Agg],
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    let _ = top;
    if !aggs
        .iter()
        .all(|a| a.func.grouped().is_some_and(|g| g.combinable()))
    {
        return Ok(None);
    }
    let exprs: Vec<&Expr> = group
        .iter()
        .chain(aggs.iter().flat_map(|a| &a.args))
        .collect();
    let Some(work) = Morsels::new(input, &exprs, ctx, outer, MORSELS_PER_WORKER)? else {
        return Ok(None);
    };
    let results = work.run(0..work.ranges.len(), true, &|wctx, range| {
        morsel(input, group, aggs, wctx, range)
    });
    // The morsels in key order: the first error is the one a single pass
    // meets first.
    let ordered = ordered_prefix(input, group);
    let mut g = Grouping::new(group, aggs);
    let mut closed = Vec::new();
    for r in results {
        match r {
            Some(Err(e)) => return Err(e),
            Some(Ok(Part::TooLarge)) => return Ok(None),
            Some(Ok(Part::Groups(keys, states))) => {
                // Rows in key order, grouped by (at least) a prefix of the
                // key: a group of an earlier morsel goes on only if it has
                // this morsel's first prefix — the others are done.
                if let (Some(at), Some(first)) = (&ordered, keys.first()) {
                    let open = |k: &[Value]| at.iter().all(|&i| k[i] == first[i]);
                    if g.seen.iter().any(|k| !open(k)) {
                        let (done, rest) = close(g, group, aggs, &open)?;
                        closed.extend(done);
                        g = rest;
                    }
                }
                let mut states: Vec<std::vec::IntoIter<Value>> =
                    states.into_iter().map(Vec::into_iter).collect();
                for key in keys {
                    let Some(gid) = g.group_of(&|| key.clone(), None) else {
                        return Ok(None); // past work_mem: the single pass partitions
                    };
                    for (acc, st) in g.accs.iter_mut().zip(&mut states) {
                        let state = st
                            .next()
                            .ok_or_else(|| Error::internal("a partial state missing"))?;
                        acc.combine(gid as usize, state)?;
                    }
                }
            }
            None => return Err(Error::internal("a morsel not aggregated")),
        }
    }
    closed.extend(finish(&mut g)?);
    PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Some(Box::new(closed.into_iter().map(Ok))))
}

/// The positions in `group` of the leading columns of the primary key
/// `input`'s scan reads in order — when `group` holds at least the first:
/// the rows of a group are then all within a run of rows sharing those.
fn ordered_prefix(input: &Plan, group: &[Expr]) -> Option<Vec<usize>> {
    let (scan, steps) = chain(input)?;
    let Plan::Scan { table, .. } = scan else {
        return None;
    };
    // A group key's column of the scan, through the projections.
    let of_scan = |e: &Expr| -> Option<usize> {
        let &Expr::Column(mut c) = e else { return None };
        for (step, _) in steps.iter().rev() {
            if let Step::Project(es) = step {
                match es.get(c)? {
                    Expr::Column(i) => c = *i,
                    _ => return None,
                }
            }
        }
        Some(c)
    };
    let cols: Vec<Option<usize>> = group.iter().map(of_scan).collect();
    let at: Vec<usize> = table
        .primary_key
        .iter()
        .map_while(|pk| cols.iter().position(|c| *c == Some(*pk)))
        .collect();
    (!at.is_empty()).then_some(at)
}

/// `g` split: the groups not `open`, finished as rows, and a grouping of the
/// open ones (their partial states carried over).
fn close<'a>(
    mut g: Grouping<'a>,
    group: &'a [Expr],
    aggs: &'a [Agg],
    open: &dyn Fn(&[Value]) -> bool,
) -> Result<(Vec<Row>, Grouping<'a>)> {
    let mut done = Vec::new();
    let mut rest = Grouping::new(group, aggs);
    for (i, key) in std::mem::take(&mut g.seen).into_iter().enumerate() {
        if open(&key) {
            let states = g
                .accs
                .iter_mut()
                .map(|a| a.partial(i))
                .collect::<Result<Vec<_>>>()?;
            let gid = rest
                .group_of(&|| key.clone(), None)
                .ok_or_else(|| Error::internal("an open group past work_mem"))?;
            for (acc, st) in rest.accs.iter_mut().zip(states) {
                acc.combine(gid as usize, st)?;
            }
        } else {
            let mut row = key;
            for a in &mut g.accs {
                row.push(a.finish(i)?);
            }
            done.push(row);
        }
    }
    Ok((done, rest))
}

/// One morsel's aggregation, on a worker.
fn morsel(input: &Plan, group: &[Expr], aggs: &[Agg], ctx: &Ctx, range: KeyRange) -> Result<Part> {
    let Some(mut pipe) = Pipeline::new(input, input, ctx, None, false, Some(range))? else {
        return Err(Error::internal("a morsel's pipeline"));
    };
    let mut g = Grouping::new(group, aggs);
    feed(&mut g, &mut pipe, Cx { ctx, outer: None })?;
    if g.parts.is_some() {
        return Ok(Part::TooLarge);
    }
    let count = g.seen.len();
    let states = g
        .accs
        .iter_mut()
        .map(|a| (0..count).map(|i| a.partial(i)).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    Ok(Part::Groups(std::mem::take(&mut g.seen), states))
}

/// Morsels per worker of a pipeline whose output becomes rows: small, as a
/// round holds a morsel's rows per worker.
const ROW_MORSELS_PER_WORKER: usize = 16;

/// A pipeline run in parallel, as rows: rounds of a morsel per worker,
/// each round's rows given in morsel order — the order, rows and errors of
/// a single pass (a `LIMIT` above stops after the round it needs).
struct ParallelRows<'a> {
    work: Morsels<'a>,
    plan: &'a Plan,
    next: usize,
    rows: std::collections::VecDeque<Row>,
    err: Option<Error>,
    done: bool,
}

impl Iterator for ParallelRows<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if let Some(r) = self.rows.pop_front() {
                return Some(Ok(r));
            }
            if let Some(e) = self.err.take() {
                self.done = true;
                return Some(Err(e));
            }
            let total = self.work.ranges.len();
            if self.done || self.next >= total {
                return None;
            }
            let end = (self.next + self.work.workers).min(total);
            let plan = self.plan;
            let results = self.work.run(self.next..end, false, &|wctx, range| {
                morsel_rows(plan, wctx, range)
            });
            self.next = end;
            for r in results {
                let (rows, err) = match r {
                    Some(Ok(x)) => x,
                    Some(Err(e)) => (Vec::new(), Some(e)),
                    None => (Vec::new(), Some(Error::internal("a morsel not run"))),
                };
                self.rows.extend(rows);
                if let Some(e) = err {
                    self.err = Some(e);
                    self.next = total;
                    break;
                }
            }
        }
    }
}

/// A batch a worker made, its selection applied: the rows, and a vector
/// per column (sent to the thread that gives the result).
type Dense = (usize, Vec<Vector>);

fn dense(b: &Batch) -> Dense {
    (
        b.sel.len(),
        b.cols.iter().map(|c| c.gather(&b.sel)).collect(),
    )
}

/// [`ParallelRows`] as batches.
struct ParallelBatches<'a> {
    work: Morsels<'a>,
    plan: &'a Plan,
    next: usize,
    batches: std::collections::VecDeque<Dense>,
    err: Option<Error>,
    done: bool,
}

impl Iterator for ParallelBatches<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Result<Batch>> {
        loop {
            if let Some((len, cols)) = self.batches.pop_front() {
                return Some(Ok(Batch {
                    len,
                    cols: cols.into_iter().map(Rc::new).collect(),
                    sel: Sel::All(len),
                }));
            }
            if let Some(e) = self.err.take() {
                self.done = true;
                return Some(Err(e));
            }
            let total = self.work.ranges.len();
            if self.done || self.next >= total {
                return None;
            }
            let end = (self.next + self.work.workers).min(total);
            let plan = self.plan;
            let results = self.work.run(self.next..end, false, &|wctx, range| {
                morsel_batches(plan, wctx, range)
            });
            self.next = end;
            for r in results {
                let (batches, err) = match r {
                    Some(Ok(x)) => x,
                    Some(Err(e)) => (Vec::new(), Some(e)),
                    None => (Vec::new(), Some(Error::internal("a morsel not run"))),
                };
                self.batches.extend(batches);
                if let Some(e) = err {
                    self.err = Some(e);
                    self.next = total;
                    break;
                }
            }
        }
    }
}

/// One morsel's batches, on a worker: the batches, and the error that
/// ended them (if one did).
fn morsel_batches(plan: &Plan, ctx: &Ctx, range: KeyRange) -> Result<(Vec<Dense>, Option<Error>)> {
    let Some(mut pipe) = Pipeline::new(plan, plan, ctx, None, false, Some(range))? else {
        return Err(Error::internal("a morsel's pipeline"));
    };
    let mut out = Vec::new();
    loop {
        match pipe.next() {
            Ok(None) => return Ok((out, None)),
            Ok(Some(Out::Batch(b))) if b.sel.is_empty() => {}
            Ok(Some(Out::Batch(b))) => out.push(dense(&b)),
            Ok(Some(Out::Rows(rows, err))) => {
                if let Some(b) = batch_of_rows(rows) {
                    out.push(dense(&b));
                }
                if err.is_some() {
                    return Ok((out, err));
                }
            }
            Err(e) => return Ok((out, Some(e))),
        }
    }
}

/// One morsel's rows, on a worker: the rows, and the error that ended
/// them (if one did).
fn morsel_rows(plan: &Plan, ctx: &Ctx, range: KeyRange) -> Result<(Vec<Row>, Option<Error>)> {
    let Some(mut pipe) = Pipeline::new(plan, plan, ctx, None, true, Some(range))? else {
        return Err(Error::internal("a morsel's pipeline"));
    };
    let mut out = Vec::new();
    loop {
        match pipe.next() {
            Ok(None) => return Ok((out, None)),
            Ok(Some(Out::Batch(b))) => out.extend(b.sel.iter().map(|i| b.row(i))),
            Ok(Some(Out::Rows(rows, err))) => {
                out.extend(rows);
                if err.is_some() {
                    return Ok((out, err));
                }
            }
            Err(e) => return Ok((out, Some(e))),
        }
    }
}

// -------------------------------------------------------------- hash joins

/// A hash join's build side: the right rows, by key.
pub(crate) struct JoinTable {
    rows: Vec<Row>,
    index: JoinIndex,
}

enum JoinIndex {
    /// One key, an integer in every right row that has one.
    Int(crate::fasthash::FastMap<i64, Vec<u32>>),
    /// Anything else.
    Any(crate::fasthash::FastMap<Vec<Value>, Vec<u32>>),
}

impl JoinTable {
    /// `rows` by their `keys` (a row with a NULL key left out: NULL equals
    /// nothing).
    fn build(rows: Vec<Row>, keys: &[Expr], ctx: &Ctx, outer: Option<&Env>) -> Result<Self> {
        let mut values = Vec::with_capacity(rows.len());
        for r in &rows {
            let e = env(r, ctx, outer);
            let k = keys
                .iter()
                .map(|k| k.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            values.push(k);
        }
        let ints = keys.len() == 1
            && values
                .iter()
                .all(|k| matches!(k[0], Value::Int(_) | Value::Null));
        let index = if ints {
            let mut m: crate::fasthash::FastMap<i64, Vec<u32>> = Default::default();
            m.reserve(rows.len());
            for (i, k) in values.iter().enumerate() {
                if let Value::Int(x) = k[0] {
                    m.entry(x).or_default().push(i as u32);
                }
            }
            JoinIndex::Int(m)
        } else {
            let mut m: crate::fasthash::FastMap<Vec<Value>, Vec<u32>> = Default::default();
            m.reserve(rows.len());
            for (i, k) in values.into_iter().enumerate() {
                if !k.iter().any(Value::is_null) {
                    m.entry(k).or_default().push(i as u32);
                }
            }
            JoinIndex::Any(m)
        };
        Ok(JoinTable { rows, index })
    }
}

/// How a hash join's left rows probe its table.
struct Probe<'a> {
    table: JoinTable,
    left_keys: &'a [Expr],
    on: Option<&'a Expr>,
    kind: crate::plan::JoinType,
    right_width: usize,
    /// Whether `on` must be checked on each candidate (not when it is just
    /// the integer keys' equality, which the table already decided).
    check: bool,
}

impl Probe<'_> {
    /// The right rows a left key matches.
    fn candidates(&self, key: &dyn Fn() -> Vec<Value>, int: Option<Option<i64>>) -> &[u32] {
        let found = match (&self.table.index, int) {
            (JoinIndex::Int(m), Some(Some(x))) => m.get(&x),
            (JoinIndex::Int(_), Some(None)) => None,
            (JoinIndex::Int(m), None) => match key().first() {
                Some(Value::Int(x)) => m.get(x),
                _ => None,
            },
            (JoinIndex::Any(m), _) => {
                let k = key();
                if k.iter().any(Value::is_null) {
                    None
                } else {
                    m.get(&k)
                }
            }
        };
        found.map_or(&[], Vec::as_slice)
    }

    /// One left row's output.
    fn emit(
        &self,
        left: &dyn Fn() -> Row,
        candidates: &[u32],
        cx: Cx,
        out: &mut Vec<Row>,
    ) -> Result<()> {
        use crate::plan::JoinType;
        let mut matched = false;
        for &r in candidates {
            let right = &self.table.rows[r as usize];
            if self.check {
                let mut combined = left();
                combined.extend_from_slice(right);
                let holds = self
                    .on
                    .map_or(Ok(true), |on| on.holds(&env(&combined, cx.ctx, cx.outer)))?;
                if !holds {
                    continue;
                }
                matched = true;
                if matches!(self.kind, JoinType::Inner | JoinType::Left) {
                    out.push(combined);
                    continue;
                }
            } else {
                matched = true;
                if matches!(self.kind, JoinType::Inner | JoinType::Left) {
                    let mut combined = left();
                    combined.extend_from_slice(right);
                    out.push(combined);
                    continue;
                }
            }
            break; // semi or anti: one match decides
        }
        match self.kind {
            JoinType::Semi if matched => out.push(left()),
            JoinType::Anti if !matched => out.push(left()),
            JoinType::Left if !matched => {
                let mut row = left();
                row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                out.push(row);
            }
            _ => {}
        }
        Ok(())
    }

    /// A batch of left rows.
    fn batch(&self, b: &Batch, cx: Cx, out: &mut Vec<Row>) -> Result<()> {
        let keys = self
            .left_keys
            .iter()
            .map(|k| eval(k, b, &b.sel, cx))
            .collect::<Result<Vec<_>>>()?;
        let ints: Option<(&[i64], &Option<Vec<bool>>)> = match (&self.table.index, &keys[..]) {
            (JoinIndex::Int(_), [k]) => match &**k {
                Vector::Int(d, n) => Some((d, n)),
                _ => None,
            },
            _ => None,
        };
        for i in b.sel.iter() {
            let key = || keys.iter().map(|k| k.value(i)).collect::<Vec<_>>();
            let int = ints.map(|(d, n)| (!is_null_at(n, i)).then(|| d[i]));
            let c = self.candidates(&key, int);
            if c.is_empty()
                && matches!(
                    self.kind,
                    crate::plan::JoinType::Inner | crate::plan::JoinType::Semi
                )
            {
                continue;
            }
            self.emit(&|| b.row(i), c, cx, out)?;
        }
        Ok(())
    }

    /// Left rows one at a time (a batch run again row by row).
    fn rows(&self, rows: Vec<Row>, cx: Cx, out: &mut Vec<Row>) -> Result<()> {
        for row in rows {
            let e = env(&row, cx.ctx, cx.outer);
            let k = self
                .left_keys
                .iter()
                .map(|k| k.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            let c = self.candidates(&|| k.clone(), None);
            self.emit(&|| row.clone(), c, cx, out)?;
        }
        Ok(())
    }

    /// A pipeline's rows probed: the output, and the error that ended it.
    fn run(&self, pipe: &mut Pipeline, cx: Cx) -> (Vec<Row>, Option<Error>) {
        let mut out = Vec::new();
        loop {
            let step = match pipe.next() {
                Ok(None) => return (out, None),
                Ok(Some(Out::Batch(b))) => self.batch(&b, cx, &mut out),
                Ok(Some(Out::Rows(rows, err))) => match self.rows(rows, cx, &mut out) {
                    Ok(()) if err.is_some() => return (out, err),
                    r => r,
                },
                Err(e) => Err(e),
            };
            if let Err(e) = step {
                return (out, Some(e));
            }
        }
    }
}

/// Whether `left` can be a vectorized hash join's probe side (a pipeline).
pub(crate) fn probes(left: &Plan) -> bool {
    crate::budget::batch_size() > 0 && chain(left).is_some()
}

/// A hash join of the pipeline `left` with the right rows `rows` (in
/// memory), vectorized: the right rows hashed once (an integer key as an
/// integer), the left side's keys evaluated a batch at a time and probed —
/// on worker threads, morsels of the left scan in key order, when it is
/// large enough; the rows, their order and errors those of the row-at-a-
/// time join.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hash_join<'a>(
    left: &'a Plan,
    rows: Vec<Row>,
    left_keys: &'a [Expr],
    right_keys: &'a [Expr],
    kind: crate::plan::JoinType,
    on: Option<&'a Expr>,
    right_width: usize,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    let table = JoinTable::build(rows, right_keys, ctx, outer)?;
    // `on` only the keys' equality: decided by an integer table.
    let lw = left.width();
    let keys_only = on.is_none_or(|on| {
        let mut parts = Vec::new();
        crate::binder::conjuncts(on.clone(), &mut parts);
        parts.iter().all(|p| {
            left_keys.iter().zip(right_keys).any(|(l, r)| {
                let r = r.map_columns(&|c| Expr::Column(c + lw));
                match p {
                    Expr::Binary(BinaryOp::Eq, a, b) => {
                        (**a == *l && **b == r) || (**a == r && **b == *l)
                    }
                    _ => false,
                }
            })
        })
    });
    let check = on.is_some() && !(keys_only && matches!(table.index, JoinIndex::Int(_)));
    let probe = Probe {
        table,
        left_keys,
        on,
        kind,
        right_width,
        check,
    };
    let mut exprs: Vec<&Expr> = left_keys.iter().collect();
    exprs.extend(on);
    if let Some(work) = Morsels::new(left, &exprs, ctx, outer, ROW_MORSELS_PER_WORKER)? {
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Box::new(ParallelJoin {
            work,
            plan: left,
            probe,
            next: 0,
            rows: std::collections::VecDeque::new(),
            err: None,
            done: false,
        }));
    }
    let Some(pipe) = Pipeline::new(left, left, ctx, outer, false, None)? else {
        return Err(Error::internal("a hash join's probe pipeline"));
    };
    Ok(Box::new(JoinRows {
        pipe: Some(pipe),
        left,
        probe,
        cx: Cx { ctx, outer },
        rows: std::collections::VecDeque::new(),
        err: None,
        measured: (Duration::ZERO, 0),
    }))
}

/// [`hash_join`] on one thread.
struct JoinRows<'a> {
    pipe: Option<Pipeline<'a>>,
    /// The probe side's plan, measured for `EXPLAIN ANALYZE`: its time and
    /// rows so far.
    left: &'a Plan,
    measured: (Duration, u64),
    probe: Probe<'a>,
    cx: Cx<'a>,
    rows: std::collections::VecDeque<Row>,
    err: Option<Error>,
}

impl Iterator for JoinRows<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if let Some(r) = self.rows.pop_front() {
                return Some(Ok(r));
            }
            if let Some(e) = self.err.take() {
                self.pipe = None;
                return Some(Err(e));
            }
            let pipe = self.pipe.as_mut()?;
            let mut out = Vec::new();
            let profile = self.cx.ctx.profile;
            let started = profile.and_then(|p| p.clock());
            let next = pipe.next();
            if let Some(p) = profile {
                if let (Some(t0), Some(t1)) = (started, p.clock()) {
                    self.measured.0 += t1.saturating_sub(t0);
                }
                self.measured.1 += match &next {
                    Ok(Some(Out::Batch(b))) => b.sel.len() as u64,
                    Ok(Some(Out::Rows(rows, _))) => rows.len() as u64,
                    _ => 0,
                };
                if matches!(next, Ok(None) | Err(_)) {
                    p.record(self.left, self.measured.0, self.measured.1, 1);
                }
            }
            let step = match next {
                Ok(None) => {
                    self.pipe = None;
                    return None;
                }
                Ok(Some(Out::Batch(b))) => self.probe.batch(&b, self.cx, &mut out),
                Ok(Some(Out::Rows(rows, err))) => {
                    let r = self.probe.rows(rows, self.cx, &mut out);
                    if r.is_ok() {
                        self.err = err;
                    }
                    r
                }
                Err(e) => Err(e),
            };
            self.rows.extend(out);
            if let Err(e) = step {
                self.err = Some(e);
            }
        }
    }
}

/// [`hash_join`] on worker threads: rounds of a morsel per worker, each
/// round's rows in morsel order.
struct ParallelJoin<'a> {
    work: Morsels<'a>,
    plan: &'a Plan,
    probe: Probe<'a>,
    next: usize,
    rows: std::collections::VecDeque<Row>,
    err: Option<Error>,
    done: bool,
}

impl Iterator for ParallelJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if let Some(r) = self.rows.pop_front() {
                return Some(Ok(r));
            }
            if let Some(e) = self.err.take() {
                self.done = true;
                return Some(Err(e));
            }
            let total = self.work.ranges.len();
            if self.done || self.next >= total {
                return None;
            }
            let end = (self.next + self.work.workers).min(total);
            let (plan, probe) = (self.plan, &self.probe);
            let results = self.work.run(self.next..end, false, &|wctx, range| {
                let Some(mut pipe) = Pipeline::new(plan, plan, wctx, None, false, Some(range))?
                else {
                    return Err(Error::internal("a morsel's pipeline"));
                };
                Ok(probe.run(
                    &mut pipe,
                    Cx {
                        ctx: wctx,
                        outer: None,
                    },
                ))
            });
            self.next = end;
            for r in results {
                let (rows, err) = match r {
                    Some(Ok(x)) => x,
                    Some(Err(e)) => (Vec::new(), Some(e)),
                    None => (Vec::new(), Some(Error::internal("a morsel not run"))),
                };
                self.rows.extend(rows);
                if let Some(e) = err {
                    self.err = Some(e);
                    self.next = total;
                    break;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ sorts

/// `ORDER BY keys` over the pipeline `input`, in parallel: each morsel's
/// rows sorted on a worker (their keys evaluated a batch at a time), the
/// sorted morsels merged ([`crate::spill::merge_sorted`]) — the order of
/// the single pass's stable sort, its first error the same.
pub(crate) fn sorted<'a>(
    input: &'a Plan,
    keys: &'a [crate::plan::SortKey],
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    if crate::budget::batch_size() == 0 {
        return Ok(None);
    }
    let exprs: Vec<&Expr> = keys.iter().map(|k| &k.expr).collect();
    let Some(work) = Morsels::new(input, &exprs, ctx, outer, MORSELS_PER_WORKER)? else {
        return Ok(None);
    };
    let flags: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
    let results = work.run(0..work.ranges.len(), true, &|wctx, range| {
        let Some(mut pipe) = Pipeline::new(input, input, wctx, None, false, Some(range))? else {
            return Err(Error::internal("a morsel's pipeline"));
        };
        let cx = Cx {
            ctx: wctx,
            outer: None,
        };
        let by_row = |row: Row, out: &mut Vec<(Vec<Value>, Row)>| -> Result<()> {
            let e = env(&row, wctx, None);
            let k = exprs
                .iter()
                .map(|x| x.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            out.push((k, row));
            Ok(())
        };
        let mut out: Vec<(Vec<Value>, Row)> = Vec::new();
        while let Some(o) = pipe.next()? {
            match o {
                Out::Batch(b) => {
                    match exprs
                        .iter()
                        .map(|x| eval(x, &b, &b.sel, cx))
                        .collect::<Result<Vec<_>>>()
                    {
                        Ok(kv) => {
                            for i in b.sel.iter() {
                                out.push((kv.iter().map(|k| k.value(i)).collect(), b.row(i)));
                            }
                        }
                        // A row at a time decides.
                        Err(_) => {
                            for i in b.sel.iter() {
                                by_row(b.row(i), &mut out)?;
                            }
                        }
                    }
                }
                Out::Rows(rows, err) => {
                    for row in rows {
                        by_row(row, &mut out)?;
                    }
                    if let Some(e) = err {
                        return Err(e);
                    }
                }
            }
        }
        out.sort_by(|(a, _), (b, _)| crate::spill::compare(&flags, a, b));
        Ok(out)
    });
    let mut parts = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Some(Ok(p)) => parts.push(p),
            Some(Err(e)) => return Err(e),
            None => return Err(Error::internal("a morsel not sorted")),
        }
    }
    PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Some(crate::spill::merge_sorted(parts, flags)?))
}
