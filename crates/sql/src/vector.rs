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
use crate::encoding::OUT_OF_LINE;
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::exec::{Entries, Rows, env};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::{Accumulator, AggRef, CallContext, GroupAccumulator};
use crate::kernels::{self, Arith, Cmp, FArith, MixState, Rhs};
use crate::plan::{Access, Agg, JoinAlgo, JoinType, Plan};
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
    /// `char(n)` values as stored (blank-padded), laid out as
    /// [`Vector::Text`]: compared without their trailing blanks.
    Char(Vec<u32>, Vec<u8>, Option<Vec<bool>>),
    /// Dates, times or timestamps as their integers (days or µs), and which
    /// are `NULL`.
    Time(TimeKind, Vec<i64>, Option<Vec<bool>>),
    /// Decimals at one scale: their mantissas (each within
    /// [`crate::decimal::MAX_DIGITS`] digits; a `NULL`'s 0), and which are
    /// `NULL`.
    Dec(u32, Mantissas, Option<Vec<bool>>),
    /// Any values.
    Values(Vec<Value>),
    /// Text or `char(n)` values by their codes in a dictionary (its
    /// entries a [`Vector::Text`] or [`Vector::Char`] without `NULL`s),
    /// and which are `NULL`: a column store's dictionary segment as it is
    /// stored.  Only between a scan and the filter that asked for it
    /// ([`DictFilter`]), which tests the entries rather than the rows.
    Dict(std::sync::Arc<Vector>, Vec<u32>, Option<Vec<bool>>),
}

/// A decimal vector's mantissas: 64-bit ones while each fits — as a column
/// segment stores them, so a cached column takes half the memory and a
/// scan reads half the bytes (§M14.20 W5) —, else 128-bit (a product's,
/// a sum's).  Read a row at a time widened ([`at`](Self::at)).
#[derive(Clone, Debug, PartialEq)]
pub enum Mantissas {
    /// Each fits 64 bits.
    Narrow(Vec<i64>),
    /// Any.
    Wide(Vec<i128>),
}

impl Mantissas {
    /// How many.
    pub fn len(&self) -> usize {
        match self {
            Mantissas::Narrow(d) => d.len(),
            Mantissas::Wide(d) => d.len(),
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `i`'s.
    #[inline]
    pub fn at(&self, i: usize) -> i128 {
        match self {
            Mantissas::Narrow(d) => i128::from(d[i]),
            Mantissas::Wide(d) => d[i],
        }
    }

    /// Narrow when every one fits.
    pub fn of(wide: Vec<i128>) -> Self {
        if wide.iter().all(|&m| i64::try_from(m).is_ok()) {
            Mantissas::Narrow(wide.into_iter().map(|m| m as i64).collect())
        } else {
            Mantissas::Wide(wide)
        }
    }

    fn slice(&self, range: std::ops::Range<usize>) -> Self {
        match self {
            Mantissas::Narrow(d) => Mantissas::Narrow(d[range].to_vec()),
            Mantissas::Wide(d) => Mantissas::Wide(d[range].to_vec()),
        }
    }

    fn pick(&self, rows: &[u32]) -> Self {
        match self {
            Mantissas::Narrow(d) => {
                Mantissas::Narrow(rows.iter().map(|&i| d[i as usize]).collect())
            }
            Mantissas::Wide(d) => Mantissas::Wide(rows.iter().map(|&i| d[i as usize]).collect()),
        }
    }

    /// The first `len` as an operand, borrowed.
    fn rows(&self, len: usize) -> DecRows<'_> {
        match self {
            Mantissas::Narrow(d) => DecRows::Narrow(&d[..len]),
            Mantissas::Wide(d) => DecRows::Col(d[..len].into()),
        }
    }
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
    pub(crate) fn of(v: &Value) -> Option<(TimeKind, i64)> {
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
pub(crate) fn nulls_from(n: Vec<bool>) -> Option<Vec<bool>> {
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
            Vector::Char(o, b, n) if !is_null_at(n, i) => Value::Char(
                String::from_utf8_lossy(&b[o[i] as usize..o[i + 1] as usize]).into_owned(),
            ),
            Vector::Time(k, d, n) if !is_null_at(n, i) => k.value(d[i]),
            Vector::Dec(s, d, n) if !is_null_at(n, i) => {
                Value::Numeric(crate::decimal::Decimal::raw(d.at(i), *s))
            }
            Vector::Values(v) => v[i].clone(),
            Vector::Dict(d, c, n) if !is_null_at(n, i) => d.value(c[i] as usize),
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
            | Vector::Char(_, _, n)
            | Vector::Time(_, _, n)
            | Vector::Dec(_, _, n)
            | Vector::Dict(_, _, n) => is_null_at(n, i),
            Vector::Values(v) => v[i].is_null(),
        }
    }

    /// A [`Vector::Dict`]'s rows as text (or `char(n)`), the bytes of only
    /// the rows `sel` (the others empty, never read); `None` for any other
    /// vector.
    pub(crate) fn undict(&self, sel: &Sel) -> Option<Vector> {
        let Vector::Dict(d, codes, nulls) = self else {
            return None;
        };
        let (Vector::Text(eo, eb, _) | Vector::Char(eo, eb, _)) = &**d else {
            return None;
        };
        let mut offsets = vec![0u32; codes.len() + 1];
        let mut bytes = Vec::new();
        let mut at = 0;
        for i in sel.iter() {
            // (The rows before `i` not in play: empty.)
            for o in &mut offsets[at + 1..=i] {
                *o = bytes.len() as u32;
            }
            if !is_null_at(nulls, i) {
                let c = codes[i] as usize;
                bytes.extend_from_slice(&eb[eo[c] as usize..eo[c + 1] as usize]);
            }
            offsets[i + 1] = bytes.len() as u32;
            at = i + 1;
        }
        for o in &mut offsets[at + 1..] {
            *o = bytes.len() as u32;
        }
        Some(d.same_text(offsets, bytes, nulls.clone()))
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
            Vector::Dec(s, d, n) => Vector::Dec(*s, d.slice(range.clone()), nulls(n)),
            Vector::Text(o, b, n) | Vector::Char(o, b, n) => {
                let (lo, hi) = (o[range.start] as usize, o[range.end] as usize);
                let offsets = o[range.start..=range.end]
                    .iter()
                    .map(|&x| x - lo as u32)
                    .collect();
                self.same_text(offsets, b[lo..hi].to_vec(), nulls(n))
            }
            Vector::Values(v) => Vector::Values(v[range].to_vec()),
            Vector::Dict(d, c, n) => Vector::Dict(d.clone(), c[range.clone()].to_vec(), nulls(n)),
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
            Vector::Dec(s, d, n) => Vector::Dec(*s, d.pick(rows), nulls(n)),
            Vector::Text(o, b, n) | Vector::Char(o, b, n) => {
                let mut offsets = Vec::with_capacity(rows.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0);
                for &i in rows {
                    let i = i as usize;
                    bytes.extend_from_slice(&b[o[i] as usize..o[i + 1] as usize]);
                    offsets.push(bytes.len() as u32);
                }
                self.same_text(offsets, bytes, nulls(n))
            }
            Vector::Values(v) => {
                Vector::Values(rows.iter().map(|&i| v[i as usize].clone()).collect())
            }
            Vector::Dict(d, c, n) => Vector::Dict(d.clone(), pick(c, rows), nulls(n)),
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
            | Vector::Char(_, _, n)
            | Vector::Time(_, _, n)
            | Vector::Dec(_, _, n)
            | Vector::Dict(_, _, n) => n.as_deref(),
            _ => None,
        }
    }

    /// A text vector of the same kind as this one (`text` or `char(n)`).
    fn same_text(&self, offsets: Vec<u32>, bytes: Vec<u8>, nulls: Option<Vec<bool>>) -> Vector {
        match self {
            Vector::Char(..) => Vector::Char(offsets, bytes, nulls),
            _ => Vector::Text(offsets, bytes, nulls),
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
            Some(first @ (Value::Text(_) | Value::Char(_)))
                if values.iter().all(|v| {
                    v.is_null() || std::mem::discriminant(v) == std::mem::discriminant(first)
                }) =>
            {
                let char = matches!(first, Value::Char(_));
                let mut offsets = Vec::with_capacity(values.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0);
                for v in &values {
                    if let Value::Text(s) | Value::Char(s) = v {
                        bytes.extend_from_slice(s.as_bytes());
                    }
                    offsets.push(bytes.len() as u32);
                }
                if char {
                    Vector::Char(offsets, bytes, nulls(&values))
                } else {
                    Vector::Text(offsets, bytes, nulls(&values))
                }
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
            Some(Value::Numeric(first))
                if values.iter().all(|v| match v {
                    Value::Numeric(d) => d.scale() == first.scale(),
                    v => v.is_null(),
                }) =>
            {
                let data = values
                    .iter()
                    .map(|v| match v {
                        Value::Numeric(d) => d.mantissa(),
                        _ => 0,
                    })
                    .collect();
                Vector::Dec(first.scale(), Mantissas::of(data), nulls(&values))
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
        // An uncorrelated subquery runs once per statement (its rows
        // cached): its value is the same for every row — evaluated here,
        // with rows in play, as a row would evaluate it.
        Expr::Subquery(s)
            if !s.correlated
                && matches!(
                    s.kind,
                    crate::expr::SubKind::Scalar
                        | crate::expr::SubKind::Exists
                        | crate::expr::SubKind::Array
                ) =>
        {
            Rc::new(Vector::Const(e.eval(&cx.env(&[]))?))
        }
        // An uncorrelated `IN (subquery)`: its rows (run once) as a list.
        Expr::Subquery(s) if !s.correlated && matches!(s.kind, crate::expr::SubKind::In(..)) => {
            let crate::expr::SubKind::In(operand, negated) = &s.kind else {
                unreachable!("matched above")
            };
            let rows = crate::expr::uncorrelated_rows(s, &cx.env(&[]))?;
            let x = eval(operand, b, sel, cx)?;
            let consts: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
            match in_consts(&x, &consts, *negated, b.len, sel) {
                Some(v) => Rc::new(v),
                None => each(b.len, sel, |i| {
                    crate::expr::in_values(&x.value(i), consts.iter().copied(), *negated)
                })?,
            }
        }
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
            // Of constants, a constant: computed once, not per row.
            if let (Vector::Const(a), Vector::Const(c)) = (&*x, &*y) {
                return Ok(Rc::new(Vector::Const(crate::expr::binary(
                    *op,
                    a.clone(),
                    c.clone(),
                )?)));
            }
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
            // A char(n) matches as stored, blanks included (as a row does).
            match (&*x, like_shape(p)) {
                (Vector::Text(o, bytes, n) | Vector::Char(o, bytes, n), Some(shape)) => {
                    let mut out = vec![false; b.len];
                    match shape {
                        LikeShape::Whole(needle) => {
                            kernels::eq_bytes(o, bytes, needle.as_bytes(), &mut out)
                        }
                        LikeShape::At(needle, a) => {
                            kernels::like_bytes(o, bytes, needle.as_bytes(), a, &mut out)
                        }
                        LikeShape::Parts(parts) => {
                            let parts: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
                            kernels::like_parts(o, bytes, &parts, &mut out)
                        }
                    }
                    if *negated {
                        out.iter_mut().for_each(|v| *v = !*v);
                    }
                    Rc::new(Vector::Bool(out, n.clone()))
                }
                _ => by_rows(e, b, sel, cx)?,
            }
        }
        Expr::InList(x, list, negated) => {
            let x = eval(x, b, sel, cx)?;
            let items = list
                .iter()
                .map(|item| eval(item, b, sel, cx))
                .collect::<Result<Vec<_>>>()?;
            Rc::new(in_list(&x, &items, *negated, b.len, sel)?)
        }
        Expr::Cast(x, ty) => {
            let x = eval(x, b, sel, cx)?;
            if let Vector::Const(v) = &*x {
                return Ok(Rc::new(Vector::Const(crate::enums::cast(
                    cx.ctx.txn,
                    v.clone(),
                    *ty,
                )?)));
            }
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
            // An immutable call of constants is a constant.
            if f.volatility() == crate::ext::Volatility::Immutable
                && let Some(vals) = args
                    .iter()
                    .map(|a| match &**a {
                        Vector::Const(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect::<Option<Vec<Value>>>()
            {
                return Ok(Rc::new(Vector::Const(
                    if f.strict() && vals.iter().any(Value::is_null) {
                        Value::Null
                    } else {
                        f.call(&call, &vals)?
                    },
                )));
            }
            // The function's batch kernel, when it has one for these.
            let refs: Vec<&Vector> = args.iter().map(|a| &**a).collect();
            if let Some(v) = f.call_batch(&call, &refs, b.len, sel) {
                return Ok(Rc::new(v?));
            }
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
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => Rc::new(case(
            operand.as_deref(),
            whens,
            otherwise.as_deref(),
            b,
            sel,
            cx,
        )?),
        // A constant (`ARRAY[0.5, 0.95]`, say): evaluated once, not per row.
        _ if crate::optimizer::constant(e) => Rc::new(Vector::Const(e.eval(&cx.env(&[]))?)),
        // The rest a row at a time.
        _ => by_rows(e, b, sel, cx)?,
    })
}

/// `e` a row at a time over the rows `sel` of `b`, each row holding only
/// the columns `e` reads (the others `NULL`: a long text it does not read
/// is not copied).
#[inline(never)]
fn by_rows(e: &Expr, b: &Batch, sel: &Sel, cx: Cx) -> Result<Rc<Vector>> {
    let reads: Vec<usize> = e
        .reads()
        .into_iter()
        .filter(|&c| c < b.cols.len())
        .collect();
    let mut row = vec![Value::Null; b.cols.len()];
    each(b.len, sel, |i| {
        for &c in &reads {
            row[c] = b.cols[c].value(i);
        }
        e.eval(&cx.env(&row))
    })
}

/// `CASE` over the rows `sel` of `b`: each `WHEN` evaluated over the rows
/// no earlier one took, its `THEN` over the rows it takes, `ELSE` over
/// the rest — what a row evaluates, and no more.
#[inline(never)]
fn case(
    operand: Option<&Expr>,
    whens: &[(Expr, Expr)],
    otherwise: Option<&Expr>,
    b: &Batch,
    sel: &Sel,
    cx: Cx,
) -> Result<Vector> {
    let operand = operand.map(|o| eval(o, b, sel, cx)).transpose()?;
    let mut out = vec![Value::Null; b.len];
    let mut rest = sel.clone();
    for (when, then) in whens {
        if rest.is_empty() {
            break;
        }
        let w = eval(when, b, &rest, cx)?;
        let hit = match &operand {
            Some(x) => {
                let mut hit = vec![false; b.len];
                for i in rest.iter() {
                    hit[i] = x.value(i).sql_eq(&w.value(i))? == Some(true);
                }
                hit
            }
            None => truths(&w, b.len, &rest)?
                .into_iter()
                .map(|t| t == 1)
                .collect(),
        };
        let taken = rest.keep(|i| hit[i]);
        if !taken.is_empty() {
            let v = eval(then, b, &taken, cx)?;
            for i in taken.iter() {
                out[i] = v.value(i);
            }
            rest = rest.keep(|i| !hit[i]);
        }
    }
    if let Some(o) = otherwise
        && !rest.is_empty()
    {
        let v = eval(o, b, &rest, cx)?;
        for i in rest.iter() {
            out[i] = v.value(i);
        }
    }
    Ok(Vector::typed(out))
}

/// How a kernel matches a `LIKE` pattern.
enum LikeShape<'p> {
    /// No wildcard: the whole string.
    Whole(&'p str),
    /// `%` only at an end, or both.
    At(&'p str, kernels::Anchor),
    /// `%` within too: the text between them.
    Parts(Vec<&'p str>),
}

/// The kernel for a `LIKE` pattern — for patterns whose only wildcard is
/// `%`, with no escapes.
fn like_shape(p: &str) -> Option<LikeShape<'_>> {
    if p.contains(['_', '\\']) {
        return None;
    }
    let lead = p.starts_with('%');
    let trail = p.len() > 1 && p.ends_with('%') || (p == "%");
    let inner = p.trim_matches('%');
    if inner.contains('%') {
        return Some(LikeShape::Parts(p.split('%').collect()));
    }
    Some(match (lead, trail) {
        (false, false) => LikeShape::Whole(inner),
        (false, true) => LikeShape::At(inner, kernels::Anchor::Prefix),
        (true, false) => LikeShape::At(inner, kernels::Anchor::Suffix),
        (true, true) => LikeShape::At(inner, kernels::Anchor::Contains),
    })
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
        let dec_like = |v: &Vector| matches!(v, Vector::Dec(..) | Vector::Const(Value::Numeric(_)));
        if (dec_like(x) || dec_like(y))
            && let (Some(a), Some(b)) = (dec_operand(x, len), dec_operand(y, len))
            && let Some(v) = dec_kernel(op, a, b, len, sel)?
        {
            return Ok(v);
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

/// A decimal operand's mantissas: a row's each (128-bit, or 64-bit read
/// widened — a narrow column's or an integer column's, not copied), or one
/// for all.
enum DecRows<'a> {
    Col(std::borrow::Cow<'a, [i128]>),
    Narrow(&'a [i64]),
    One(i128),
}

impl<'a> DecRows<'a> {
    #[inline]
    fn at(&self, i: usize) -> i128 {
        match self {
            DecRows::Col(d) => d[i],
            DecRows::Narrow(d) => i128::from(d[i]),
            DecRows::One(m) => *m,
        }
    }

    /// Each mantissa times `f` (raised to a larger scale), and whether that
    /// overflowed, by row (`None`: no row did).  Times 1, as they are.
    fn raised(self, f: i128, len: usize) -> (DecRows<'a>, Option<Vec<bool>>) {
        if f == 1 {
            return (self, None);
        }
        let column = |m: &mut dyn Iterator<Item = i128>| {
            let mut over = vec![false; len];
            let out = m
                .zip(&mut over)
                .map(|(m, o)| {
                    m.checked_mul(f).unwrap_or_else(|| {
                        *o = true;
                        0
                    })
                })
                .collect::<Vec<_>>();
            let over = over.contains(&true).then_some(over);
            (DecRows::Col(out.into()), over)
        };
        match self {
            DecRows::One(m) => match m.checked_mul(f) {
                Some(r) => (DecRows::One(r), None),
                None => (DecRows::One(0), Some(vec![true; len])),
            },
            DecRows::Col(d) => column(&mut d[..len].iter().copied()),
            DecRows::Narrow(d) => column(&mut d[..len].iter().map(|&m| i128::from(m))),
        }
    }
}

/// A decimal operand: its mantissas, their scale, and its `NULL`s — an
/// integer a decimal of scale 0, as a row widens it ([`Decimal::from_i64`]);
/// `None` for anything else.
///
/// [`Decimal::from_i64`]: crate::decimal::Decimal::from_i64
fn dec_operand(v: &Vector, len: usize) -> Option<(DecRows<'_>, u32, Option<&[bool]>)> {
    match v {
        Vector::Dec(s, d, n) => Some((d.rows(len), *s, n.as_deref())),
        Vector::Int(d, n) => Some((DecRows::Narrow(&d[..len]), 0, n.as_deref())),
        Vector::Const(Value::Numeric(d)) => Some((DecRows::One(d.mantissa()), d.scale(), None)),
        Vector::Const(Value::Int(i)) => Some((DecRows::One(i128::from(*i)), 0, None)),
        _ => None,
    }
}

/// `x op y` on decimals, as [`crate::decimal::Decimal`] computes it a row
/// at a time: `+` and `-` at the larger scale (raising the other side
/// first), `*` at the sum of the scales, a result past the digits a
/// decimal may have an error on a row in play; a comparison at the larger
/// scale.  `None`: not one it computes (`/`, `%`, or a comparison whose
/// raising overflows — the digits compare then).
fn dec_kernel(
    op: BinaryOp,
    (a, sa, an): (DecRows, u32, Option<&[bool]>),
    (b, sb, bn): (DecRows, u32, Option<&[bool]>),
    len: usize,
    sel: &Sel,
) -> Result<Option<Vector>> {
    use crate::decimal::{LIMIT, MAX_DIGITS, overflow, pow10};
    use BinaryOp::*;
    if !matches!(op, Add | Sub | Mul) && !is_cmp(op) {
        return Ok(None);
    }
    let nulls = either_null(an, bn, len);
    let live = |i: usize| !nulls.as_ref().is_some_and(|n| n[i]);
    let fits = |r: Option<i128>| r.filter(|r| r.unsigned_abs() <= LIMIT);
    if op == Mul {
        let s = sa + sb;
        let mut out = vec![0i128; len];
        let mut over = false;
        for (i, o) in out.iter_mut().enumerate() {
            match fits(a.at(i).checked_mul(b.at(i))) {
                Some(r) => *o = r,
                None => over = true,
            }
        }
        let past = |i: usize| s > MAX_DIGITS || fits(a.at(i).checked_mul(b.at(i))).is_none();
        if (over || s > MAX_DIGITS) && sel.iter().any(|i| live(i) && past(i)) {
            return Err(overflow());
        }
        return Ok(Some(Vector::Dec(
            s.min(MAX_DIGITS),
            Mantissas::Wide(out),
            nulls,
        )));
    }
    // At the larger scale (at most `MAX_DIGITS`: a power of ten that fits).
    let s = sa.max(sb);
    let up = |d: u32| pow10(s - d).expect("within the digits");
    let (a, a_over) = a.raised(up(sa), len);
    let (b, b_over) = b.raised(up(sb), len);
    let raised_over =
        |i: usize| a_over.as_ref().is_some_and(|o| o[i]) || b_over.as_ref().is_some_and(|o| o[i]);
    let any_over = a_over.is_some() || b_over.is_some();
    if is_cmp(op) {
        if any_over && sel.iter().any(|i| live(i) && raised_over(i)) {
            return Ok(None);
        }
        let want = |o: std::cmp::Ordering| match op {
            Eq => o.is_eq(),
            NotEq => o.is_ne(),
            Lt => o.is_lt(),
            LtEq => o.is_le(),
            Gt => o.is_gt(),
            _ => o.is_ge(),
        };
        let out = (0..len).map(|i| want(a.at(i).cmp(&b.at(i)))).collect();
        return Ok(Some(Vector::Bool(out, nulls)));
    }
    let mut out = vec![0i128; len];
    let mut over = any_over;
    for (i, o) in out.iter_mut().enumerate() {
        let r = if op == Add {
            a.at(i).checked_add(b.at(i))
        } else {
            a.at(i).checked_sub(b.at(i))
        };
        match fits(r) {
            Some(r) => *o = r,
            None => over = true,
        }
    }
    if over {
        let past = |i: usize| {
            raised_over(i)
                || fits(if op == Add {
                    a.at(i).checked_add(b.at(i))
                } else {
                    a.at(i).checked_sub(b.at(i))
                })
                .is_none()
        };
        if sel.iter().any(|i| live(i) && past(i)) {
            return Err(overflow());
        }
    }
    Ok(Some(Vector::Dec(s, Mantissas::Wide(out), nulls)))
}

/// A text or `char(n)` vector's layout, and whether it is `char(n)` (its
/// rows compared without their trailing blanks).
pub(crate) fn text_view(v: &Vector) -> Option<(&[u32], &[u8], &Option<Vec<bool>>, bool)> {
    match v {
        Vector::Text(o, b, n) => Some((o, b, n, false)),
        Vector::Char(o, b, n) => Some((o, b, n, true)),
        _ => None,
    }
}

/// A text or `char(n)` constant's bytes as a comparison sees them.
fn text_const(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Text(s) => Some(s.as_bytes()),
        Value::Char(s) => Some(crate::types::unpadded(s).as_bytes()),
        _ => None,
    }
}

/// Row `i` of a text layout, a `char(n)`'s without its trailing blanks.
pub(crate) fn text_at<'b>(o: &[u32], b: &'b [u8], char: bool, i: usize) -> &'b [u8] {
    let s = &b[o[i] as usize..o[i + 1] as usize];
    if char {
        let end = s.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
        &s[..end]
    } else {
        s
    }
}

/// `x op y` for a text or `char(n)` vector `x` and text `y` (a vector or a
/// constant) — a `char(n)` side without its trailing blanks, as a row
/// compares it.
fn cmp_text(op: BinaryOp, x: &Vector, y: &Vector, len: usize) -> Result<Option<Vector>> {
    let Some((o, b, xn, xc)) = text_view(x) else {
        return Ok(None);
    };
    let mut out = vec![false; len];
    let k = cmp_of(op);
    let nulls = match y {
        Vector::Const(c) if text_const(c).is_some() => {
            let c = text_const(c).expect("text");
            if !xc && matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                kernels::eq_bytes(o, b, c, &mut out);
                if op == BinaryOp::NotEq {
                    out.iter_mut().for_each(|v| *v = !*v);
                }
            } else {
                for (i, v) in out.iter_mut().enumerate() {
                    *v = cmp_holds(k, text_at(o, b, xc, i).cmp(c));
                }
            }
            xn.clone()
        }
        y => {
            let Some((p, c, yn, yc)) = text_view(y) else {
                return Ok(None);
            };
            for (i, v) in out.iter_mut().enumerate() {
                *v = cmp_holds(k, text_at(o, b, xc, i).cmp(text_at(p, c, yc, i)));
            }
            either_null(xn.as_deref(), yn.as_deref(), len)
        }
    };
    Ok(Some(Vector::Bool(out, nulls)))
}

/// `x [NOT] IN (consts)` by a kernel — a text or integer vector, and
/// values of its kind, none `NULL` (a `NULL` item makes a miss `NULL`) —
/// the values in a hash set when there are many; `None` when it cannot.
fn in_consts(
    x: &Vector,
    consts: &[&Value],
    negated: bool,
    len: usize,
    sel: &Sel,
) -> Option<Vector> {
    /// More items than this are looked up in a hash set.
    const SCAN: usize = 8;
    if consts.iter().any(|c| c.is_null()) {
        return None;
    }
    let mut out = vec![false; len];
    let nulls = if let Some((o, b, n, char)) = text_view(x) {
        let needles: Vec<&[u8]> = consts
            .iter()
            .map(|c| text_const(c))
            .collect::<Option<_>>()?;
        if needles.len() > SCAN {
            let set: std::collections::HashSet<&[u8], crate::fasthash::FastBuild> =
                needles.into_iter().collect();
            for i in sel.iter() {
                out[i] = set.contains(text_at(o, b, char, i));
            }
        } else {
            for i in sel.iter() {
                out[i] = needles.contains(&text_at(o, b, char, i));
            }
        }
        n.clone()
    } else if let Vector::Int(d, n) = x {
        let ints: Vec<i64> = consts
            .iter()
            .map(|c| {
                if let Value::Int(i) = c {
                    Some(*i)
                } else {
                    None
                }
            })
            .collect::<Option<_>>()?;
        if ints.len() > SCAN {
            let set: std::collections::HashSet<i64, crate::fasthash::FastBuild> =
                ints.into_iter().collect();
            for i in sel.iter() {
                out[i] = set.contains(&d[i]);
            }
        } else {
            for i in sel.iter() {
                out[i] = ints.contains(&d[i]);
            }
        }
        n.clone()
    } else {
        return None;
    };
    if negated {
        out.iter_mut().for_each(|v| *v = !*v);
    }
    Some(Vector::Bool(out, nulls))
}

/// `x [NOT] IN (items)` over the rows `sel`, the items evaluated as
/// vectors: kernels for a text, `char(n)` or integer `x` against
/// constants of its kind, else a value at a time — `x` and the items only,
/// as a row compares them.
fn in_list(
    x: &Vector,
    items: &[Rc<Vector>],
    negated: bool,
    len: usize,
    sel: &Sel,
) -> Result<Vector> {
    let consts: Option<Vec<&Value>> = items
        .iter()
        .map(|v| match &**v {
            Vector::Const(c) => Some(c),
            _ => None,
        })
        .collect();
    if let Some(v) = consts.and_then(|c| in_consts(x, &c, negated, len, sel)) {
        return Ok(v);
    }
    let out = each(len, sel, |i| {
        crate::expr::in_values(
            &x.value(i),
            items.iter().map(|v| v.value(i)).collect::<Vec<_>>().iter(),
            negated,
        )
    })?;
    Ok(Rc::try_unwrap(out).unwrap_or_else(|v| (*v).clone()))
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

/// How many of the `n` rows of the columns `cols` `p` holds for,
/// evaluated as a batch (the statistics' estimates on a sample).
pub(crate) fn count_true(p: &Expr, cols: Vec<Vector>, n: usize, ctx: &Ctx) -> Result<usize> {
    let b = Batch {
        len: n,
        cols: cols.into_iter().map(Rc::new).collect(),
        sel: Sel::All(n),
    };
    Ok(filter(p, b, Cx { ctx, outer: None })?.sel.len())
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

/// The rows of `b` whose key columns pass their filters.
fn keep_keys(filters: &[(usize, std::sync::Arc<crate::keyfilter::KeyFilter>)], b: Batch) -> Batch {
    let mut sel = b.sel;
    for (c, f) in filters {
        let v = &b.cols[*c];
        sel = match &**v {
            Vector::Int(d, n) => sel.keep(|i| !is_null_at(n, i) && f.contains(d[i])),
            v => sel.keep(|i| f.holds(&v.value(i))),
        };
    }
    Batch { sel, ..b }
}

static DICT_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many dictionaries' entries this process has tested a filter's
/// conjunct on ([`DictFilter`]; for tests and diagnostics).
pub fn dictionary_tests() -> u64 {
    DICT_TESTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A filter right above a scan whose leading conjuncts each test one text
/// column against constants, and cannot fail (PLAN.md §M14.20 V13): on a
/// dictionary segment's rows ([`Vector::Dict`]) a conjunct is tested on
/// the dictionary's entries — once a segment — and the rows whose entry
/// it holds FALSE for are dropped before the filter runs: they would
/// leave at it, an `AND` whose left part is false reading no further (so
/// no error of a later conjunct is skipped).  The dictionaries' text is
/// then made for the rows left only, and the filter runs as it would.
struct DictFilter<'a> {
    /// The step it goes before.
    step: usize,
    /// The conjuncts: the column each tests, and it.
    preds: Vec<(usize, &'a Expr)>,
    /// The filter's other conjuncts, in order (`None`: none) — what is
    /// left to test of a row every conjunct above held TRUE for.
    rest: Option<Expr>,
    /// Each conjunct's last dictionary, and its entries' truth (1, 0;
    /// -1: `NULL`).
    seen: std::cell::RefCell<Vec<Option<(std::sync::Arc<Vector>, Rc<Vec<i8>>)>>>,
}

impl<'a> DictFilter<'a> {
    /// The one for `steps`: their first filter, if only key filters come
    /// before it, and its leading conjuncts that qualify.
    fn of(steps: &[(Step<'a>, &'a Plan)]) -> Option<Self> {
        let (step, p) = steps.iter().enumerate().find_map(|(i, (s, _))| match s {
            Step::Keys(_) => None,
            Step::Filter(p) => Some(Some((i, *p))),
            Step::Project(_) | Step::Probe(_) => Some(None),
        })??;
        let mut preds = Vec::new();
        Self::leading(p, &mut preds);
        if preds.is_empty() {
            return None;
        }
        let mut all = Vec::new();
        crate::binder::conjuncts(p.clone(), &mut all);
        let rest = all
            .into_iter()
            .skip(preds.len())
            .reduce(|a, b| Expr::Binary(BinaryOp::And, Box::new(a), Box::new(b)));
        Some(DictFilter {
            step,
            seen: std::cell::RefCell::new(vec![None; preds.len()]),
            preds,
            rest,
        })
    }

    /// The conjuncts of `e` that qualify, from the first on (whether all
    /// did).
    fn leading(e: &'a Expr, out: &mut Vec<(usize, &'a Expr)>) -> bool {
        if let Expr::Binary(BinaryOp::And, l, r) = e {
            return Self::leading(l, out) && Self::leading(r, out);
        }
        let mut col = None;
        match Self::tests(e, &mut col) {
            true if let Some(c) = col => {
                out.push((c, e));
                true
            }
            _ => false,
        }
    }

    /// Whether `e` tests the one column `col` against constants, by what
    /// cannot fail (comparisons, `IN`, `LIKE`, `IS NULL`, `NOT`, `AND`,
    /// `OR`).
    fn tests(e: &Expr, col: &mut Option<usize>) -> bool {
        use BinaryOp::*;
        match e {
            Expr::Column(c) => *col.get_or_insert(*c) == *c,
            Expr::Literal(_) => true,
            Expr::Binary(Eq | NotEq | Lt | LtEq | Gt | GtEq | And | Or, a, b) => {
                Self::tests(a, col) && Self::tests(b, col)
            }
            Expr::Unary(crate::ast::UnaryOp::Not, a) | Expr::IsNull(a, _) => Self::tests(a, col),
            Expr::InList(a, list, _) => {
                list.iter().all(|x| matches!(x, Expr::Literal(_))) && Self::tests(a, col)
            }
            Expr::Like { expr, pattern, .. } => {
                matches!(**pattern, Expr::Literal(_)) && Self::tests(expr, col)
            }
            _ => false,
        }
    }

    /// The text columns it tests.
    fn columns(&self) -> Vec<usize> {
        let mut cols: Vec<usize> = self.preds.iter().map(|(c, _)| *c).collect();
        cols.sort_unstable();
        cols.dedup();
        cols
    }

    /// `b` with the rows a conjunct's entry holds FALSE for dropped, and
    /// its dictionaries made text — gathered into a batch of their own
    /// when at most half are left (the kernels after it then compute
    /// those alone).
    ///
    /// With it, whether every conjunct was tested so and held TRUE for
    /// every row left (then only [`rest`](Self::rest) remains to test).
    fn apply(&self, mut b: Batch, cx: Cx) -> (Batch, bool) {
        let mut dropped = false;
        let mut decided = true;
        if !b.sel.is_empty() {
            for (k, (c, e)) in self.preds.iter().enumerate() {
                let v = b.cols[*c].clone();
                let Vector::Dict(d, codes, nulls) = &*v else {
                    decided = false;
                    continue;
                };
                let Some(truth) = self.truth(k, d, *c, e, b.cols.len(), cx) else {
                    decided = false;
                    continue;
                };
                let mut unsure = false;
                b.sel = b.sel.keep(|i| {
                    if is_null_at(nulls, i) {
                        unsure = true;
                        return true;
                    }
                    match truth[codes[i] as usize] {
                        0 => false,
                        1 => true,
                        _ => {
                            unsure = true;
                            true
                        }
                    }
                });
                decided &= !unsure;
                dropped = true;
            }
        }
        if dropped && b.sel.len() * 2 <= b.len {
            let n = b.sel.len();
            let cols = b
                .cols
                .iter()
                .map(|c| {
                    let v = c.gather(&b.sel);
                    Rc::new(v.undict(&Sel::All(n)).unwrap_or(v))
                })
                .collect();
            let b = Batch {
                len: n,
                cols,
                sel: Sel::All(n),
            };
            return (b, decided);
        }
        for col in &mut b.cols {
            if let Some(v) = col.undict(&b.sel) {
                *col = Rc::new(v);
            }
        }
        (b, decided)
    }

    /// Conjunct `k`'s truth on dictionary `d`'s entries (`None` when it
    /// fails on them: then the filter alone decides).
    fn truth(
        &self,
        k: usize,
        d: &std::sync::Arc<Vector>,
        col: usize,
        e: &Expr,
        width: usize,
        cx: Cx,
    ) -> Option<Rc<Vec<i8>>> {
        let mut seen = self.seen.borrow_mut();
        if let Some((last, truth)) = &seen[k]
            && std::sync::Arc::ptr_eq(last, d)
        {
            return Some(truth.clone());
        }
        let (Vector::Text(o, _, _) | Vector::Char(o, _, _)) = &**d else {
            return None;
        };
        let m = o.len() - 1;
        let entries = Batch {
            len: m,
            cols: (0..width)
                .map(|j| {
                    Rc::new(if j == col {
                        (**d).clone()
                    } else {
                        Vector::Const(Value::Null)
                    })
                })
                .collect(),
            sel: Sel::All(m),
        };
        let r = eval(e, &entries, &entries.sel, cx).ok()?;
        let truth: Rc<Vec<i8>> = Rc::new(
            (0..m)
                .map(|i| match r.value(i) {
                    Value::Bool(true) => 1,
                    Value::Bool(false) => 0,
                    _ => -1,
                })
                .collect(),
        );
        seen[k] = Some((d.clone(), truth.clone()));
        DICT_TESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(truth)
    }
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
    Char,
    Time(TimeKind),
    Values,
    Enum,
    /// Decimals: as values, then one scale's mantissas when they share one.
    Numeric,
}

/// A vector being built.
enum Builder {
    Skip,
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Text(Vec<u32>, Vec<u8>, Vec<bool>),
    Char(Vec<u32>, Vec<u8>, Vec<bool>),
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
            Kind::Text | Kind::Char => {
                let mut offsets = Vec::with_capacity(cap + 1);
                offsets.push(0);
                let bytes = Vec::with_capacity(cap * 16);
                if kind == Kind::Char {
                    Builder::Char(offsets, bytes, nulls())
                } else {
                    Builder::Text(offsets, bytes, nulls())
                }
            }
            Kind::Time(k) => Builder::Time(k, Vec::with_capacity(cap), nulls()),
            Kind::Values | Kind::Enum | Kind::Numeric => Builder::Values(Vec::with_capacity(cap)),
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
            Builder::Text(o, b, n) | Builder::Char(o, b, n) => {
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
            (Builder::Text(o, b, n), Value::Text(s)) | (Builder::Char(o, b, n), Value::Char(s)) => {
                Self::push_text(o, b, n, s.as_bytes())
            }
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
            | Builder::Char(_, _, n)
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
            Builder::Char(o, b, n) => Vector::Char(o, b, nulls_from(n)),
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
    /// The values stored out of line, read beside the rows.
    large: crate::large::Chunks<'a>,
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
                    DataType::Char => Kind::Char,
                    DataType::Date => Kind::Time(TimeKind::Date),
                    DataType::Time => Kind::Time(TimeKind::Time),
                    DataType::Timestamp => Kind::Time(TimeKind::Timestamp),
                    DataType::TimestampTz => Kind::Time(TimeKind::TimestampTz),
                    DataType::Enum(_) => Kind::Enum,
                    DataType::Numeric => Kind::Numeric,
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
            large: crate::large::Chunks::new(
                scan.txn,
                scan.table,
                scan.validated,
                scan.end.as_deref(),
            ),
        })
    }
}

impl crate::storage::BatchSource for RowBatches<'_> {
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>> {
        let mut cols: Vec<Builder> = self.kinds.iter().map(|&k| Builder::new(k, max)).collect();
        // Past the last column wanted, a row is not read at all.
        let upto = cols
            .iter()
            .rposition(|c| !matches!(c, Builder::Skip))
            .map_or(0, |c| c + 1);
        let mut n = 0;
        while n < max {
            let Some((key, bytes)) = self.entries.next()? else {
                break;
            };
            decode_into(&mut self.large, self.table, key, bytes, &mut cols[..upto])?;
            n += 1;
        }
        if n == 0 {
            return Ok(None);
        }
        let mut out: Vec<Rc<Vector>> = Vec::with_capacity(cols.len());
        for (c, b) in cols.into_iter().enumerate() {
            let mut v = b.finish();
            if self.kinds[c] == Kind::Numeric
                && let Vector::Values(vals) = v
            {
                v = Vector::typed(vals);
            }
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
    large: &mut crate::large::Chunks,
    t: &Table,
    key: &[u8],
    b: &[u8],
    cols: &mut [Builder],
) -> Result<()> {
    if cols.is_empty() {
        return Ok(()); // no column read (`count(*)`): the row is not looked at
    }
    let bad = |_| Error::internal("malformed row encoding");
    let h = crate::encoding::row_header(b)?;
    let n = h.n;
    let mut r = h.reader();
    let mut at = 0;
    for (c, col) in cols.iter_mut().enumerate().take(n) {
        // Columns not read are passed over, to the offset before the next
        // one read when the row has one (stored past the columns wanted, or
        // the table's: not read).
        if matches!(col, Builder::Skip) {
            continue;
        }
        if at != c {
            h.seek(&mut at, &mut r, c)?;
        }
        at = c + 1;
        let tag = r.peek();
        if tag == Some(OUT_OF_LINE) {
            r.u8().map_err(bad)?;
            let len = r.u32().map_err(bad)?;
            let pk = crate::exec::pk_suffix(t, key);
            col.push(large.fetch(t, pk, c, len)?);
            continue;
        }
        match (col, tag) {
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
            (Builder::Text(o, bytes, nulls), Some(4))
            | (Builder::Char(o, bytes, nulls), Some(18)) => {
                r.u8().map_err(bad)?;
                let len = r.u32().map_err(bad)? as usize;
                Builder::push_text(o, bytes, nulls, r.take(len).map_err(bad)?);
            }
            (Builder::Text(o, bytes, nulls), Some(22))
            | (Builder::Char(o, bytes, nulls), Some(23)) => {
                r.u8().map_err(bad)?;
                let len = usize::from(r.u8().map_err(bad)?);
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
    /// The hash joins' key filters on the scan (`crate::keyfilter`):
    /// each column's.
    Keys(Vec<(usize, std::sync::Arc<crate::keyfilter::KeyFilter>)>),
    /// A hash join's probe of its table (made before the pipeline runs:
    /// [`prepare`]).
    Probe(Box<Probe<'a>>),
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
    /// Its first filter's tests of dictionaries' entries.
    dict: Option<DictFilter<'a>>,
}

/// Whether `plan` is a chain the vectorized executor runs as a pipeline
/// (a scan through filters and projections).
pub(crate) fn pipelined(plan: &Plan) -> bool {
    chain(plan, None).is_some()
}

/// The scan, and the filters and projections above it, of a chain that
/// ends in a table scan by the primary key (or every row) — and the hash
/// joins probing `tables` above it, for their left sides; `None` for
/// anything else.
fn chain<'p>(
    plan: &'p Plan,
    tables: Option<&Tables>,
) -> Option<(&'p Plan, Vec<(Step<'p>, &'p Plan)>)> {
    match plan {
        Plan::Join {
            left,
            right,
            kind,
            on,
            algo:
                JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    build_left: false,
                    ..
                },
        } if probing(*kind) => {
            let Some(Built::Table(table)) = tables?.get(&address(plan)) else {
                return None;
            };
            let (scan, mut steps) = chain(left, tables)?;
            let probe = Probe::new(
                table.clone(),
                left.width(),
                left_keys,
                right_keys,
                *kind,
                on.as_ref(),
                right.width(),
            );
            steps.push((Step::Probe(Box::new(probe)), plan));
            Some((scan, steps))
        }
        Plan::Scan { access, filter, .. } => {
            if access.secondary().is_some() {
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
            let (scan, mut steps) = chain(input, tables)?;
            steps.push((Step::Filter(predicate), plan));
            Some((scan, steps))
        }
        Plan::Project { input, exprs } => {
            let (scan, mut steps) = chain(input, tables)?;
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
            Step::Keys(filters) => out.extend(filters.iter().map(|(c, f)| f.bound(*c))),
            Step::Project(_) | Step::Probe(_) => break,
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
    /// rows (so a chain without a filter gains nothing — but a morsel of
    /// a parallel run, `range`).
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
        let Some((scan, mut steps)) = chain(plan, Some(&ctx.join_tables.borrow())) else {
            return Ok(None);
        };
        // The key filters first: what they drop no step above sees.
        let keys = crate::keyfilter::of_scan(ctx, scan);
        if !keys.is_empty() {
            steps.insert(0, (Step::Keys(keys), scan));
        }
        if rows
            && range.is_none()
            && !steps
                .iter()
                .any(|(s, _)| matches!(s, Step::Filter(_) | Step::Keys(_) | Step::Probe(_)))
        {
            return Ok(None);
        }
        let Plan::Scan { table, access, .. } = scan else {
            unreachable!("a chain ends in a scan")
        };
        let cx = Cx { ctx, outer };
        let dict = DictFilter::of(&steps);
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
                    dicts: dict.as_ref().map(DictFilter::columns).unwrap_or_default(),
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
            dict,
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
        let mut steps = self.steps.iter().enumerate().peekable();
        if steps
            .peek()
            .is_none_or(|(_, (_, n))| !std::ptr::eq(*n, self.scan))
        {
            record(self.scan, &b);
        }
        while let Some((i, (step, node))) = steps.next() {
            let mut decided = None;
            if let Some(d) = &self.dict
                && d.step == i
            {
                let (tested, all) = d.apply(b, self.cx);
                b = tested;
                decided = all.then_some(&d.rest);
            }
            b = match (step, decided) {
                // Its leading conjuncts TRUE for every row left.
                (Step::Filter(_), Some(None)) => b,
                (Step::Filter(_), Some(Some(rest))) => filter(rest, b, self.cx)?,
                (Step::Filter(p), None) => filter(p, b, self.cx)?,
                (Step::Project(exprs), _) => project(exprs, b, self.cx)?,
                (Step::Keys(filters), _) => keep_keys(filters, b),
                (Step::Probe(p), _) => p.apply(b, self.cx)?,
            };
            if steps
                .peek()
                .is_none_or(|(_, (_, n))| !std::ptr::eq(*n, *node))
            {
                record(node, &b);
            }
        }
        Ok(b)
    }

    /// `input` through the steps a row at a time (after an error on the
    /// batch: a row at a time decides).
    fn redo(&self, input: &Batch) -> Out {
        let mut rows = Vec::new();
        for i in input.sel.iter() {
            if let Err(e) = self.through(input.row(i), 0, &mut rows) {
                return Out::Rows(rows, Some(e));
            }
        }
        Out::Rows(rows, None)
    }

    /// `row` through the steps from `from` on, the rows it gives appended
    /// to `out` (a join's, each in turn, before the error that stopped
    /// it).
    fn through(&self, mut row: Row, from: usize, out: &mut Vec<Row>) -> Result<()> {
        for (k, (step, _)) in self.steps.iter().enumerate().skip(from) {
            match step {
                Step::Keys(filters) => {
                    if !crate::keyfilter::passes(filters, &row) {
                        return Ok(());
                    }
                }
                Step::Filter(p) => {
                    if !p.holds(&self.cx.env(&row))? {
                        return Ok(());
                    }
                }
                Step::Project(exprs) => {
                    let e = self.cx.env(&row);
                    row = exprs.iter().map(|x| x.eval(&e)).collect::<Result<Row>>()?;
                }
                Step::Probe(p) => {
                    let mut joined = Vec::new();
                    let r = p.rows(vec![row], self.cx, &mut joined);
                    for j in joined {
                        self.through(j, k + 1, out)?;
                    }
                    return r;
                }
            }
        }
        out.push(row);
        Ok(())
    }
}

/// Whether the chain `plan` gains from running on workers: it filters or
/// joins, or many of its rows are read (no `LIMIT` stops it within
/// [`SERIAL_ROWS`]: decoding them is the work).
fn filters_or_probes(plan: &Plan, ctx: &Ctx) -> bool {
    let bounded = ctx
        .bounds
        .borrow()
        .get(&(plan as *const Plan as usize))
        .is_some_and(|&k| k <= SERIAL_ROWS);
    chain(plan, Some(&ctx.join_tables.borrow())).is_some_and(|(_, steps)| {
        !bounded
            || steps
                .iter()
                .any(|(s, _)| matches!(s, Step::Filter(_) | Step::Probe(_)))
    })
}

/// `plan` run vectorized, as rows — when it can be (see [`Pipeline`]).
pub fn rows<'a>(
    plan: &'a Plan,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Option<Rows<'a>>> {
    let tables = prepare(plan, ctx, outer)?;
    let filters = filters_or_probes(plan, ctx);
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
        tables.keep();
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
    let tables = prepare(plan, ctx, outer)?;
    let filters = filters_or_probes(plan, ctx);
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
        tables.keep();
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
    Int(crate::keytable::IntKeys, Option<u32>),
    /// Any keys: found by their hashes, compared with `seen`'s.
    Any(crate::keytable::KeyTable),
}

/// A vectorized aggregate's state.
struct Grouping<'a> {
    group: &'a [Expr],
    aggs: &'a [Agg],
    keys: Keys,
    /// Each group's key, in the order the groups were first seen.
    seen: Vec<Vec<Value>>,
    accs: Vec<Box<dyn GroupAccumulator>>,
    /// The groups the accumulators have room for.
    room: usize,
    bytes: usize,
    /// The bytes a group's states take, as the accumulators tell
    /// ([`GroupAccumulator::group_bytes`]).
    state_bytes: usize,
    limit: usize,
    parts: Option<crate::spill::Partitions>,
}

use crate::keytable::KeyRef;

/// Room for at least `n` groups in `accs`, which have room for `room`:
/// grown by half again (not a group at a time, a call an accumulator).
fn make_room(accs: &mut [Box<dyn GroupAccumulator>], room: &mut usize, n: usize) {
    if n > *room {
        *room = n.max(*room + *room / 2).max(16);
        for a in accs {
            a.resize(*room);
        }
    }
}

impl<'a> Grouping<'a> {
    fn new(group: &'a [Expr], aggs: &'a [Agg]) -> Self {
        let mut g = Grouping {
            group,
            aggs,
            keys: match group.len() {
                0 => Keys::Total,
                _ => Keys::Any(Default::default()),
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
            room: 0,
            bytes: 0,
            state_bytes: 0,
            limit: crate::budget::work_mem(),
            parts: None,
        };
        g.state_bytes = g.accs.iter().map(|a| a.group_bytes()).sum();
        g
    }

    /// The group of the row with key `key` (made if new), or `None` when
    /// the row goes to a partition (past work_mem, as a row would).
    fn group_of(&mut self, key: KeyRef, int: Option<Option<i64>>) -> Option<u32> {
        // An integer key: looked up and placed in one probe.
        if let (Keys::Int(m, _), Some(Some(i))) = (&mut self.keys, int) {
            if let Some(g) = m.get(i) {
                return Some(g);
            }
            let size = crate::spill::INT_KEY_BYTES + self.state_bytes;
            if self.parts.is_some()
                || (self.bytes + size > self.limit && crate::spill::MAX_LEVEL > 0)
            {
                return None;
            }
            self.bytes += size;
            let g = self.seen.len() as u32;
            m.insert(i, g);
            self.seen.push(vec![Value::Int(i)]);
            make_room(&mut self.accs, &mut self.room, self.seen.len());
            return Some(g);
        }
        let mut hash = 0;
        let found = match (&self.keys, int) {
            (Keys::Total, _) => self.seen.first().map(|_| 0),
            (Keys::Int(_, null), Some(None)) => *null,
            (Keys::Any(t), _) => {
                hash = key.hash(&t.state);
                t.find(hash, |g| key.eq(&self.seen[g as usize]))
            }
            (Keys::Int(..), _) => unreachable!("an integer key"),
        };
        if found.is_some() {
            return found;
        }
        let key = key.to_vec();
        let size = crate::spill::row_bytes(&key) + self.state_bytes;
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
            (Keys::Int(_, null), _) => *null = Some(g),
            (Keys::Any(t), _) => t.insert(hash),
        }
        self.seen.push(key);
        make_room(&mut self.accs, &mut self.room, self.seen.len());
        Some(g)
    }

    /// Switches integer keys to any keys.
    fn widen(&mut self) {
        if let Keys::Int(..) = self.keys {
            let mut t = crate::keytable::KeyTable::default();
            for k in &self.seen {
                t.insert(KeyRef::Values(k).hash(&t.state));
            }
            self.keys = Keys::Any(t);
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
                .group_of(KeyRef::Values(&[]), None)
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
                    self.keys = Keys::Int(Default::default(), None);
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
            let key = KeyRef::Batch(keys, i);
            let int = int_key.then(|| (!keys[0].is_null(i)).then(|| keys[0].value(i)));
            let int = int.map(|v| match v {
                Some(Value::Int(x)) => Some(x),
                _ => None,
            });
            match self.group_of(key, int) {
                Some(g) => {
                    rows.push(i as u32);
                    groups.push(g);
                }
                None => self
                    .parts
                    .get_or_insert_with(|| crate::spill::Partitions::new(0))
                    .push(&key.to_vec(), &row(i))?,
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
        // A state's kind may follow its first rows.
        self.state_bytes = self.accs.iter().map(|a| a.group_bytes()).sum();
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

/// Feeds a pipeline's rows to `g`, calling `after` after each batch.
fn feed<'a>(
    g: &mut Grouping<'a>,
    pipe: &mut Pipeline,
    cx: Cx,
    after: &mut dyn FnMut(&mut Grouping<'a>) -> Result<()>,
) -> Result<()> {
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
        after(g)?;
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
    let tables = prepare(input, ctx, outer)?;
    if let Some(rows) = parallel_aggregate(input, top, group, aggs, ctx, outer)? {
        return Ok(Some(rows));
    }
    let Some(mut pipe) = Pipeline::new(input, top, ctx, outer, false, None)? else {
        tables.keep();
        return Ok(None);
    };
    drop(tables);
    let cx = Cx { ctx, outer };
    let mut g = Grouping::new(group, aggs);
    feed(&mut g, &mut pipe, cx, &mut |_| Ok(()))?;
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

/// The parts of the keys' hashes the morsels' groups are merged by, on
/// the workers (ClickHouse's two-level tables have 256).
const MERGE_PARTS: usize = 64;

/// Partial groups from this many on are merged on the workers.
const PARALLEL_MERGE: usize = 16_384;

/// The merge part of key `k`: bits of its hash a hash table's slots and
/// tags do not lean on.
fn merge_part(hasher: &MixState, k: &[Value]) -> usize {
    use std::hash::BuildHasher;
    ((hasher.hash_one(k) >> 32) as usize) & (MERGE_PARTS - 1)
}

/// A morsel's groups of one merge part: their keys, each aggregate's
/// partial states, and where the morsel first saw each.
struct Bucket {
    keys: Vec<Vec<Value>>,
    states: Vec<Vec<Value>>,
    places: Vec<u32>,
    /// Each group's morsel, first seen there (with its place there, its
    /// rank); none for an ordered morsel's edges (their morsel's).
    morsels: Vec<u32>,
}

/// The morsels' groups (in key order) merged on the workers, a merge part
/// each, and finished: the rows a single pass gives, in its order — each
/// group where it was first seen (its first morsel, its place there), the
/// first error by that order.  A morsel's groups in scratch ([`Spilled`])
/// are read back by part, before those it kept.  Rows past half of
/// `work_mem` in all come a part at a time instead, each part's in that
/// order — those that did not fit beside the others' from scratch.
fn merge_parallel(
    cells: &[Vec<std::sync::Mutex<Option<Bucket>>>],
    spills: &[Option<Spilled>],
    width: usize,
    aggs: &[Agg],
    work: &Morsels,
    spend: &Spend,
) -> Result<Rows<'static>> {
    // (A rank: the morsel, the place there.  An error's: 0 while merging —
    // every merge comes before any finish — 1 while finishing.)
    type Rank = (u32, u32);
    /// A part's rows: in memory with their ranks, or in scratch.
    enum Merged {
        Memory(Vec<(Rank, Row)>),
        Run(std::sync::Arc<crate::spill::Sealed>),
    }
    type Done = (Merged, usize, Option<((u8, Rank), Error)>);
    let (ctx, shared, workers) = (work.ctx, work.shared, work.workers);
    let results = on_workers(ctx, shared, workers, MERGE_PARTS, false, &|_,
                                                                         b|
     -> Result<
        Done,
    > {
        let mut map: HashMap<Vec<Value>, u32, MixState> = HashMap::default();
        let mut ranks: Vec<Rank> = Vec::new();
        let mut accs: Vec<Box<dyn GroupAccumulator>> = aggs
            .iter()
            .map(|a| a.func.grouped().expect("combinable"))
            .collect();
        let per_group: usize = accs.iter().map(|a| a.group_bytes()).sum();
        let mut size = 0;
        let mut room = 0;
        let mut add = |m: u32,
                       key: Vec<Value>,
                       i: u32,
                       states: &mut dyn Iterator<Item = Value>|
         -> Option<((u8, Rank), Error)> {
            let next = ranks.len() as u32;
            let mut fresh = false;
            let gid = *map.entry(key).or_insert_with_key(|k| {
                fresh = true;
                size += crate::spill::row_bytes(k) + per_group;
                next
            });
            if fresh {
                ranks.push((m, i));
                make_room(&mut accs, &mut room, ranks.len());
            } else {
                // (The parts come by worker, not in rank order.)
                let r = &mut ranks[gid as usize];
                *r = (*r).min((m, i));
            }
            for acc in accs.iter_mut() {
                let st = states.next().unwrap_or(Value::Null);
                if let Err(e) = acc.combine(gid as usize, st) {
                    return Some(((0, (m, i)), e));
                }
            }
            None
        };
        for (c, cell) in cells[b].iter().enumerate() {
            if let Some(sp) = &spills[c] {
                for flush in &sp.flushes {
                    let (from, to) = flush[b];
                    for row in sp.run.range(from, to) {
                        let mut row = row?;
                        let states = row.split_off(1 + width);
                        let Value::Int(rank) = row[0] else {
                            return Err(Error::internal("a spilled group's place"));
                        };
                        row.remove(0);
                        let (m, i) = ((rank >> 32) as u32, rank as u32);
                        if let Some(err) = add(m, row, i, &mut states.into_iter()) {
                            return Ok((Merged::Memory(Vec::new()), 0, Some(err)));
                        }
                    }
                }
            }
            let Some(bucket) = cell.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                continue;
            };
            let mut states: Vec<std::vec::IntoIter<Value>> =
                bucket.states.into_iter().map(Vec::into_iter).collect();
            for ((key, &i), &m) in bucket
                .keys
                .into_iter()
                .zip(&bucket.places)
                .zip(&bucket.morsels)
            {
                let mut row_states = states.iter_mut().map(|s| s.next().unwrap_or(Value::Null));
                if let Some(err) = add(m, key, i, &mut row_states) {
                    return Ok((Merged::Memory(Vec::new()), 0, Some(err)));
                }
            }
        }
        let mut keys: Vec<Vec<Value>> = vec![Vec::new(); ranks.len()];
        for (k, g) in map {
            keys[g as usize] = k;
        }
        // In first-seen order: by rank (the parts came by morsel or by
        // worker, so the numbering may not be).
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_unstable_by_key(|&g| ranks[g]);
        let mut rows = Vec::with_capacity(keys.len());
        let mut failed = None;
        for g in order {
            let mut row = std::mem::take(&mut keys[g]);
            for a in &mut accs {
                match a.finish(g) {
                    Ok(v) => row.push(v),
                    Err(e) => {
                        failed = Some(((1, ranks[g]), e));
                        break;
                    }
                }
            }
            if failed.is_some() {
                break;
            }
            rows.push((ranks[g], row));
        }
        if failed.is_some() || spend.keep(&spend.merged, size) {
            return Ok((Merged::Memory(rows), size, failed));
        }
        let mut run = crate::spill::Run::new()?;
        for (_, row) in rows {
            run.push(&row)?;
        }
        Ok((Merged::Run(run.seal()?), size, None))
    });
    let mut parts = Vec::with_capacity(MERGE_PARTS);
    let mut total = 0;
    let mut first_err: Option<((u8, Rank), Error)> = None;
    for r in results {
        let (merged, size, err) = match r {
            Some(r) => r?,
            None => return Err(Error::internal("a merge part not run")),
        };
        total += size;
        parts.push(merged);
        if let Some((rank, e)) = err
            && first_err.as_ref().is_none_or(|(r, _)| rank < *r)
        {
            first_err = Some((rank, e));
        }
    }
    if let Some((_, e)) = first_err {
        return Err(e);
    }
    if total <= spend.half {
        // All in memory: in the order a single pass gives.
        let mut all = Vec::new();
        for p in parts {
            match p {
                Merged::Memory(rows) => all.extend(rows),
                Merged::Run(_) => return Err(Error::internal("a merge part in scratch")),
            }
        }
        all.sort_unstable_by_key(|(rank, _)| *rank);
        return Ok(Box::new(all.into_iter().map(|(_, row)| Ok(row))));
    }
    Ok(Box::new(parts.into_iter().flat_map(
        |p| -> Box<dyn Iterator<Item = Result<Row>>> {
            match p {
                Merged::Memory(rows) => Box::new(rows.into_iter().map(|(_, row)| Ok(row))),
                Merged::Run(run) => Box::new(run.iter()),
            }
        },
    )))
}

static PARALLEL_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many aggregates and pipelines this process has run in parallel
/// (for tests and diagnostics).
pub fn parallel_runs() -> u64 {
    PARALLEL_RUNS.load(std::sync::atomic::Ordering::Relaxed)
}

static ADAPTIVE_LOOKUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many adaptive joins this process has run as lookups
/// ([`crate::plan::Adaptive`]; for tests and diagnostics).
pub fn adaptive_lookups() -> u64 {
    ADAPTIVE_LOOKUPS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn note_adaptive_lookups() {
    ADAPTIVE_LOOKUPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

static SPILLED_GROUPINGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many parallel aggregates this process has run with groups written
/// to scratch by their morsels (for tests and diagnostics).
pub fn spilled_groupings() -> u64 {
    SPILLED_GROUPINGS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts a run on worker threads made outside this module (tests).
pub(crate) fn note_parallel_run() {
    PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// What one morsel's aggregation gives the merge.
enum Part {
    /// Its groups, a merge part of the keys' hashes at a time
    /// ([`MERGE_PARTS`]; all in one when there is no `GROUP BY`) — those
    /// past its share of `work_mem` in scratch.
    Groups {
        buckets: Vec<Bucket>,
        spilled: Option<Spilled>,
    },
    /// (Rows in key order, grouped by a prefix of the key.)  Its groups no
    /// other morsel can have — of neither its first prefix nor its last —
    /// finished, each with its place, up to the first that fails, and that
    /// one's error; the others as partial states.
    Ordered {
        rows: Vec<(u32, Row)>,
        failed: Option<(u32, Error)>,
        edges: Bucket,
    },
    /// Its groups outgrew work_mem.
    TooLarge,
}

/// Whether `e` gives the same values on any thread: no subqueries (they
/// would run once per worker), no function that is not
/// [`parallel_safe`](crate::ext::ScalarFunction::parallel_safe) (a
/// volatile one's values may depend on the order of calls).
pub(crate) fn parallel_safe(e: &Expr) -> bool {
    !e.has_subquery() && !parallel_unsafe_call(e)
}

/// [`parallel_safe`] for `e` run under `ctx`: its subqueries allowed where
/// [`crate::expr::ready_subqueries`] readies them — the uncorrelated ones
/// run once here first (their rows cached, which the workers are given),
/// the correlated ones run by the workers for each row.
pub(crate) fn parallel_safe_in(e: &Expr, ctx: &Ctx) -> bool {
    !parallel_unsafe_call(e) && (!e.has_subquery() || crate::expr::ready_subqueries(e, ctx))
}

/// Whether `e` calls a function that is not parallel-safe (outside its
/// subqueries).
fn parallel_unsafe_call(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Call(f, _) = x
            && !f.parallel_safe()
        {
            found = true;
        }
    });
    found
}

/// Whether `access` reads one key of a unique index (the primary key's,
/// or a unique one's) — a row at most, nothing to share out.
fn one_key(table: &Table, access: &Access) -> bool {
    let Access::Key { index, prefix, .. } = access else {
        return false;
    };
    match index {
        None => !table.primary_key.is_empty() && prefix.len() == table.primary_key.len(),
        Some(i) => table.indexes[*i].unique && prefix.len() == table.indexes[*i].columns.len(),
    }
}

/// A chain's scan cut into morsels for worker threads, and what the
/// workers share.
struct Morsels<'a> {
    shared: &'a dyn nucleus_kv::KvRead,
    ctx: &'a Ctx<'a>,
    ranges: Vec<KeyRange>,
    workers: usize,
    /// The join tables its pipelines probe, for the workers' contexts.
    tables: Shared,
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
        let Some((scan, steps)) = chain(input, Some(&ctx.join_tables.borrow())) else {
            return Ok(None);
        };
        let Plan::Scan { table, access, .. } = scan else {
            unreachable!("a chain ends in a scan")
        };
        if one_key(table, access) {
            return Ok(None);
        }
        let mut all: Vec<&Expr> = exprs.to_vec();
        for (step, _) in &steps {
            match step {
                Step::Filter(p) => all.push(p),
                Step::Project(es) => all.extend(es.iter()),
                Step::Probe(p) => all.extend(p.left_keys.iter().chain(p.on)),
                Step::Keys(_) => {}
            }
        }
        if !all.iter().all(|e| parallel_safe_in(e, ctx)) {
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
            tables: snapshot(&ctx.join_tables.borrow()),
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
        let (ranges, first) = (&self.ranges, which.start);
        on_workers_kept(
            self.ctx,
            self.shared,
            self.workers,
            which.len(),
            stop,
            Kept3 {
                init: &|| (),
                each: &|wctx, _, m| f(wctx, ranges[first + m].clone()),
                end: &|_| (),
                tables: &self.tables,
            },
        )
        .0
    }

    /// [`Morsels::run`] over all the morsels with a state each worker keeps
    /// across the morsels it takes (`init`, then `f` for each — given the
    /// morsel's number —, then `end`, on the worker): the morsels' results
    /// in morsel order, and the workers' ends.  A worker takes its morsels
    /// in increasing order.
    fn run_kept<S, T: Send, U: Send>(
        &self,
        init: &(dyn Fn() -> S + Sync),
        f: &(dyn Fn(&Ctx, &mut S, usize, KeyRange) -> Result<T> + Sync),
        end: &(dyn Fn(S) -> U + Sync),
    ) -> (Vec<Option<Result<T>>>, Vec<U>) {
        let ranges = &self.ranges;
        on_workers_kept(
            self.ctx,
            self.shared,
            self.workers,
            ranges.len(),
            true,
            Kept3 {
                init,
                each: &|wctx, st, m| f(wctx, st, m, ranges[m].clone()),
                end,
                tables: &self.tables,
            },
        )
    }
}

/// `f` over the items `0..n`, on up to `workers` threads, each with a
/// context of its own over the shared reads `shared` of `ctx`'s
/// transaction and the statement's ambient state (who, the time, the
/// budget; the rows read counted for the statement); the results in item
/// order.  `stop`: after an item fails, the later ones need not run
/// (theirs are `None`).
pub(crate) fn on_workers<T: Send>(
    ctx: &Ctx,
    shared: &dyn nucleus_kv::KvRead,
    workers: usize,
    n: usize,
    stop: bool,
    f: &(dyn Fn(&Ctx, usize) -> Result<T> + Sync),
) -> Vec<Option<Result<T>>> {
    on_workers_kept(
        ctx,
        shared,
        workers,
        n,
        stop,
        Kept3 {
            init: &|| (),
            each: &|wctx, _, m| f(wctx, m),
            end: &|_| (),
            tables: &Shared::new(),
        },
    )
    .0
}

/// What a worker does with the state it keeps ([`on_workers_kept`]): makes
/// it, gives it each item, and turns it into what the worker gives back.
pub(crate) struct Kept3<'f, S, T, U> {
    pub(crate) init: &'f (dyn Fn() -> S + Sync),
    pub(crate) each: &'f (dyn Fn(&Ctx, &mut S, usize) -> Result<T> + Sync),
    pub(crate) end: &'f (dyn Fn(S) -> U + Sync),
    /// The join tables the items' pipelines probe.
    pub(crate) tables: &'f Shared,
}

/// [`on_workers`] with a state each worker keeps across the items it takes
/// (made by `init` on the worker, given to `f` with each item, and turned
/// by `end` into what the worker gives back): the items' results in item
/// order, and the workers' ends in no order.  A worker takes its items in
/// increasing order.
pub(crate) fn on_workers_kept<S, T: Send, U: Send>(
    ctx: &Ctx,
    shared: &dyn nucleus_kv::KvRead,
    workers: usize,
    n: usize,
    stop: bool,
    kept: Kept3<'_, S, T, U>,
) -> (Vec<Option<Result<T>>>, Vec<U>) {
    let Kept3 {
        init,
        each: f,
        end,
        tables,
    } = kept;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
    let ambient = crate::parallel::Ambient::capture();
    // The uncorrelated subqueries' rows run so far (`parallel_safe_in`),
    // and the hash joins' key filters.
    let cache = ctx.cache.borrow().clone();
    let key_filters = ctx.key_filters.borrow().clone();
    let (registry, params, needed, validated, memoize) = (
        ctx.registry,
        ctx.params,
        &ctx.needed,
        ctx.validated,
        ctx.memoize,
    );
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(usize::MAX);
    let scanned = AtomicU64::new(0);
    let results: Vec<Mutex<Option<Result<T>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let ends: Mutex<Vec<U>> = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..workers.min(n) {
            sc.spawn(|| {
                let (u, rows) = ambient.run(|| {
                    let mut state = init();
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
                        if !cache.is_empty() {
                            *wctx.cache.get_mut() = cache.clone();
                        }
                        if !key_filters.is_empty() {
                            *wctx.key_filters.get_mut() = key_filters.clone();
                        }
                        if !tables.is_empty() {
                            *wctx.join_tables.get_mut() = tables
                                .iter()
                                .map(|(j, t)| (*j, Built::Table(t.clone())))
                                .collect();
                        }
                        let r = f(&wctx, &mut state, m);
                        if r.is_err() {
                            failed.fetch_min(m, SeqCst);
                        }
                        *results[m].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    }
                    end(state)
                });
                ends.lock().unwrap_or_else(|e| e.into_inner()).push(u);
                scanned.fetch_add(rows, SeqCst);
            });
        }
    });
    crate::budget::add_scanned(scanned.into_inner());
    let results = results
        .into_iter()
        .map(|r| r.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect();
    (
        results,
        ends.into_inner().unwrap_or_else(|e| e.into_inner()),
    )
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
    // The morsels in key order: the first error is the one a single pass
    // meets first.  (Groups in key order merge as they come: one bucket a
    // morsel.)
    let ordered = ordered_prefix(input, group);
    let parts = if ordered.is_none() && !group.is_empty() {
        MERGE_PARTS
    } else {
        1
    };
    let spend = Spend::new(work.workers);
    let spend = (parts == MERGE_PARTS).then_some(&spend);
    let results: Vec<Option<Result<Part>>> = match spend {
        // A table a worker ([`Kept`]).
        Some(spend) if aggs.iter().all(|a| a.func.order_free()) => {
            let (morsels, ends) = work.run_kept(
                &|| Kept::new(group, aggs),
                &|wctx, kept, m, range| kept.feed(input, wctx, (m as u32, range), spend),
                &|kept| kept.end(spend),
            );
            // A morsel's error first, in morsel order; then a worker's, the
            // one whose first morsel comes first.
            let mut parts = Vec::new();
            for r in morsels {
                match r {
                    Some(Err(e)) => return Err(e),
                    Some(Ok(handed)) => parts.extend(handed.map(Ok)),
                    None => return Err(Error::internal("a morsel not aggregated")),
                }
            }
            let mut ends = ends;
            ends.sort_by_key(|(first, _)| *first);
            parts.extend(ends.into_iter().map(|(_, part)| part));
            parts.into_iter().map(Some).collect()
        }
        _ => {
            work.run_kept(
                &|| (),
                &|wctx, _, m, range| {
                    morsel(
                        input,
                        group,
                        aggs,
                        wctx,
                        (m as u32, range),
                        ordered.as_deref(),
                        spend,
                    )
                },
                &|_| (),
            )
            .0
        }
    };
    if ordered.is_some() {
        let rows = merge_ordered(results, aggs)?;
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(rows.map(|rows| -> Rows<'a> { Box::new(rows.into_iter().map(Ok)) }));
    }
    // Many groups, in no order of their keys: merged on the workers, a part
    // of the keys' hashes each.
    let (partials, spilled) = results.iter().fold((0, false), |(n, s), r| match r {
        Some(Ok(Part::Groups { buckets, spilled })) => (
            n + buckets.iter().map(|b| b.keys.len()).sum::<usize>(),
            s || spilled.is_some(),
        ),
        _ => (n, s),
    });
    if let Some(spend) = spend
        && (partials >= PARALLEL_MERGE || spilled)
    {
        // Each merge part's buckets, a morsel's each, in key order.
        let mut cells: Vec<Vec<std::sync::Mutex<Option<Bucket>>>> =
            (0..MERGE_PARTS).map(|_| Vec::new()).collect();
        let mut spills = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Some(Err(e)) => return Err(e),
                Some(Ok(Part::TooLarge)) => return Ok(None),
                Some(Ok(Part::Groups { buckets, spilled })) => {
                    for (cell, b) in cells.iter_mut().zip(buckets) {
                        cell.push(std::sync::Mutex::new(Some(b)));
                    }
                    spills.push(spilled);
                }
                Some(Ok(Part::Ordered { .. })) => {
                    return Err(Error::internal("an ordered morsel unordered"));
                }
                None => return Err(Error::internal("a morsel not aggregated")),
            }
        }
        if spilled {
            SPILLED_GROUPINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let rows = merge_parallel(&cells, &spills, group.len(), aggs, &work, spend)?;
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Some(rows));
    }
    // Few: merged here, in the order of their ranks (a single pass's).
    type Entry = ((u32, u32), Vec<Value>, Vec<Value>);
    let mut all: Vec<Entry> = Vec::new();
    for r in results {
        match r {
            Some(Err(e)) => return Err(e),
            Some(Ok(Part::TooLarge)) => return Ok(None),
            Some(Ok(Part::Ordered { .. })) => {
                return Err(Error::internal("an ordered morsel unordered"));
            }
            Some(Ok(Part::Groups {
                spilled: Some(_), ..
            })) => {
                return Err(Error::internal("spilled groups merged in one table"));
            }
            Some(Ok(Part::Groups { buckets, .. })) => {
                for b in buckets {
                    let mut states: Vec<std::vec::IntoIter<Value>> =
                        b.states.into_iter().map(Vec::into_iter).collect();
                    for ((key, place), m) in b.keys.into_iter().zip(b.places).zip(b.morsels) {
                        let st = states
                            .iter_mut()
                            .map(|s| s.next().unwrap_or(Value::Null))
                            .collect();
                        all.push(((m, place), key, st));
                    }
                }
            }
            None => return Err(Error::internal("a morsel not aggregated")),
        }
    }
    all.sort_unstable_by_key(|(rank, _, _)| *rank);
    let mut g = Grouping::new(group, aggs);
    for (_, key, states) in all {
        let Some(gid) = g.group_of(KeyRef::Values(&key), None) else {
            return Ok(None); // past work_mem: the single pass partitions
        };
        for (acc, state) in g.accs.iter_mut().zip(states) {
            acc.combine(gid as usize, state)?;
        }
    }
    let rows = finish(&mut g)?;
    PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Some(Box::new(rows.into_iter().map(Ok))))
}

/// The ordered morsels' parts merged: their finished groups, and their
/// edge groups combined here (a few a morsel) and finished — the rows of a
/// single pass in its order (each group where first seen), and its error:
/// a morsel's own first (rows are fed before any group finishes), then a
/// combining one, then the first group by that order to fail finishing.
fn merge_ordered(results: Vec<Option<Result<Part>>>, aggs: &[Agg]) -> Result<Option<Vec<Row>>> {
    type Rank = (u32, u32);
    let mut parts = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Some(Err(e)) => return Err(e),
            Some(Ok(Part::TooLarge)) => return Ok(None),
            Some(Ok(p)) => parts.push(p),
            None => return Err(Error::internal("a morsel not aggregated")),
        }
    }
    let mut out: Vec<(Rank, Row)> = Vec::new();
    let mut failed: Option<(Rank, Error)> = None;
    let mut map: HashMap<Vec<Value>, u32, MixState> = HashMap::default();
    let mut ranks: Vec<Rank> = Vec::new();
    let mut accs: Vec<Box<dyn GroupAccumulator>> = aggs
        .iter()
        .map(|a| a.func.grouped().expect("combinable"))
        .collect();
    let mut room = 0;
    for (m, p) in parts.into_iter().enumerate() {
        let m = m as u32;
        let Part::Ordered {
            rows,
            failed: f,
            edges,
        } = p
        else {
            return Err(Error::internal("an unordered morsel ordered"));
        };
        out.extend(rows.into_iter().map(|(i, r)| ((m, i), r)));
        if let Some((i, e)) = f
            && failed.is_none()
        {
            failed = Some(((m, i), e));
        }
        let mut states: Vec<std::vec::IntoIter<Value>> =
            edges.states.into_iter().map(Vec::into_iter).collect();
        for (key, &i) in edges.keys.into_iter().zip(&edges.places) {
            let next = ranks.len() as u32;
            let mut fresh = false;
            let gid = *map.entry(key).or_insert_with(|| {
                fresh = true;
                next
            });
            if fresh {
                ranks.push((m, i));
                make_room(&mut accs, &mut room, ranks.len());
            }
            for (acc, st) in accs.iter_mut().zip(&mut states) {
                acc.combine(gid as usize, st.next().unwrap_or(Value::Null))?;
            }
        }
    }
    let mut keys: Vec<Vec<Value>> = vec![Vec::new(); ranks.len()];
    for (k, g) in map {
        keys[g as usize] = k;
    }
    'groups: for (g, key) in keys.into_iter().enumerate() {
        let mut row = key;
        for a in &mut accs {
            match a.finish(g) {
                Ok(v) => row.push(v),
                Err(e) => {
                    if failed.as_ref().is_none_or(|(r, _)| ranks[g] < *r) {
                        failed = Some((ranks[g], e));
                    }
                    continue 'groups;
                }
            }
        }
        out.push((ranks[g], row));
    }
    if let Some((_, e)) = failed {
        return Err(e);
    }
    // Mostly in order already: runs a merge sort takes as they are.
    out.sort_by_key(|(rank, _)| *rank);
    Ok(Some(out.into_iter().map(|(_, row)| row).collect()))
}

/// The positions in `group` of the leading columns of the primary key
/// `input`'s scan reads in order — when `group` holds at least the first:
/// the rows of a group are then all within a run of rows sharing those.
fn ordered_prefix(input: &Plan, group: &[Expr]) -> Option<Vec<usize>> {
    let (scan, steps) = chain(input, None)?;
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

/// An ordered morsel's groups ([`Part::Ordered`]): those of its first
/// and last prefix (`at`: the prefix's positions in the key) as partial
/// states, the others finished.
/// (A partial state that fails fails the morsel, as an update would.)
fn edges(mut g: Grouping, aggs: usize, at: &[usize]) -> Result<Part> {
    let seen = std::mem::take(&mut g.seen);
    let mut edges = Bucket {
        keys: Vec::new(),
        states: vec![Vec::new(); aggs],
        places: Vec::new(),
        morsels: Vec::new(),
    };
    let (Some(first), Some(last)) = (seen.first().cloned(), seen.last().cloned()) else {
        return Ok(Part::Ordered {
            rows: Vec::new(),
            failed: None,
            edges,
        });
    };
    let same = |a: &[Value], b: &[Value]| at.iter().all(|&i| a[i] == b[i]);
    let mut rows = Vec::with_capacity(seen.len());
    let mut failed = None;
    for (i, key) in seen.into_iter().enumerate() {
        let place = i as u32;
        if same(&key, &first) || same(&key, &last) {
            for (a, st) in g.accs.iter_mut().zip(&mut edges.states) {
                st.push(a.partial(i)?);
            }
            edges.keys.push(key);
            edges.places.push(place);
        } else if failed.is_none() {
            let mut row = key;
            for a in &mut g.accs {
                match a.finish(i) {
                    Ok(v) => row.push(v),
                    Err(e) => {
                        failed = Some((place, e));
                        break;
                    }
                }
            }
            if failed.is_none() {
                rows.push((place, row));
            }
        }
    }
    Ok(Part::Ordered {
        rows,
        failed,
        edges,
    })
}

/// An unordered parallel aggregate's memory, `work_mem` (DuckDB's
/// radix-partitioned aggregation; Kuiper et al., ICDE 2024): the running
/// morsels' groups and those the morsels done kept — past it, a morsel
/// holding its fair share or more writes its groups to scratch
/// ([`flush`]) and goes on with none; a morsel done keeps its groups in
/// memory while those kept fit in half, and the merged parts' rows are
/// kept within the other half.
struct Spend {
    limit: usize,
    half: usize,
    /// A running morsel's fair share: half among the workers.
    share: usize,
    running: std::sync::atomic::AtomicUsize,
    kept: std::sync::atomic::AtomicUsize,
    merged: std::sync::atomic::AtomicUsize,
}

impl Spend {
    fn new(workers: usize) -> Self {
        let limit = crate::budget::work_mem();
        Spend {
            limit,
            half: limit / 2,
            share: limit / (2 * workers.max(1)),
            running: Default::default(),
            kept: Default::default(),
            merged: Default::default(),
        }
    }

    /// A running morsel's groups grown from `from` to `to` bytes: whether
    /// it is to write them to scratch.
    fn grown(&self, from: usize, to: usize) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let running = self.running.fetch_add(to - from, Relaxed) + to - from;
        to >= self.share && running + self.kept.load(Relaxed) > self.limit
    }

    /// A running morsel's `bytes` no longer held.
    fn release(&self, bytes: usize) {
        self.running
            .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether `bytes` more fit in `of`'s half (then counted).
    fn keep(&self, of: &std::sync::atomic::AtomicUsize, bytes: usize) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        of.try_update(Relaxed, Relaxed, |k| {
            (k + bytes <= self.half).then_some(k + bytes)
        })
        .is_ok()
    }
}

/// A morsel's groups written to scratch past its share of `work_mem`: a
/// flush at a time, each flush's groups by merge part — rows of a group's
/// place in the morsel, its key and its partial states — and each part's
/// byte range in the run.
struct Spilled {
    run: std::sync::Arc<crate::spill::Sealed>,
    flushes: Vec<Vec<(u64, u64)>>,
}

/// A [`Spilled`] being written.
struct Spilling {
    run: crate::spill::Run,
    flushes: Vec<Vec<(u64, u64)>>,
}

/// `g`'s groups as partial states, split into `parts` buckets by their
/// keys' hashes; each group's place `base` on from where `g` saw it, and
/// its morsel the one `morsel_of` gives for that place.
fn buckets(
    g: &mut Grouping,
    parts: usize,
    base: u32,
    morsel_of: &dyn Fn(u32) -> u32,
) -> Result<Vec<Bucket>> {
    let count = g.seen.len();
    let states = g
        .accs
        .iter_mut()
        .map(|a| (0..count).map(|i| a.partial(i)).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    let seen = std::mem::take(&mut g.seen);
    if parts == 1 {
        let places: Vec<u32> = (base..base + seen.len() as u32).collect();
        return Ok(vec![Bucket {
            morsels: places.iter().map(|&p| morsel_of(p)).collect(),
            places,
            keys: seen,
            states,
        }]);
    }
    let mut buckets: Vec<Bucket> = (0..parts)
        .map(|_| Bucket {
            keys: Vec::new(),
            states: vec![Vec::new(); g.aggs.len()],
            places: Vec::new(),
            morsels: Vec::new(),
        })
        .collect();
    let hasher = MixState::default();
    let mut states: Vec<std::vec::IntoIter<Value>> =
        states.into_iter().map(Vec::into_iter).collect();
    for (i, k) in seen.into_iter().enumerate() {
        let b = merge_part(&hasher, &k);
        let bucket = &mut buckets[b];
        for (to, from) in bucket.states.iter_mut().zip(&mut states) {
            to.push(
                from.next()
                    .ok_or_else(|| Error::internal("a partial state missing"))?,
            );
        }
        bucket.keys.push(k);
        let place = base + i as u32;
        bucket.places.push(place);
        bucket.morsels.push(morsel_of(place));
    }
    Ok(buckets)
}

/// Writes `g`'s groups to `out` (a flush, by merge part; each group's rank
/// as its morsel and place — `base` on, then moved past them — one
/// integer) and empties `g`.
fn flush(
    g: &mut Grouping,
    out: &mut Option<Spilling>,
    base: &mut u32,
    morsel_of: &dyn Fn(u32) -> u32,
) -> Result<()> {
    if g.seen.is_empty() {
        return Ok(());
    }
    let w = match out {
        Some(w) => w,
        None => out.insert(Spilling {
            run: crate::spill::Run::new()?,
            flushes: Vec::new(),
        }),
    };
    let n = g.seen.len() as u32;
    let mut ranges = Vec::with_capacity(MERGE_PARTS);
    let mut row = Vec::new();
    for b in buckets(g, MERGE_PARTS, *base, morsel_of)? {
        let from = w.run.at();
        let mut states: Vec<std::vec::IntoIter<Value>> =
            b.states.into_iter().map(Vec::into_iter).collect();
        for ((key, place), m) in b.keys.into_iter().zip(b.places).zip(b.morsels) {
            row.clear();
            row.push(Value::Int((i64::from(m) << 32) | i64::from(place)));
            row.extend(key);
            for st in &mut states {
                row.push(st.next().unwrap_or(Value::Null));
            }
            w.run.push(&row)?;
        }
        ranges.push((from, w.run.at()));
    }
    w.flushes.push(ranges);
    *base += n;
    *g = Grouping::new(g.group, g.aggs);
    g.limit = usize::MAX;
    Ok(())
}

/// One morsel's aggregation, on a worker.
/// (With `spend` its groups are split into [`MERGE_PARTS`] buckets by
/// their keys' hashes, and past its share go to scratch; else into one.)
fn morsel(
    input: &Plan,
    group: &[Expr],
    aggs: &[Agg],
    ctx: &Ctx,
    (m, range): (u32, KeyRange),
    ordered: Option<&[usize]>,
    spend: Option<&Spend>,
) -> Result<Part> {
    let morsel_of = |_: u32| m;
    let parts = if spend.is_some() { MERGE_PARTS } else { 1 };
    let Some(mut pipe) = Pipeline::new(input, input, ctx, None, false, Some(range))? else {
        return Err(Error::internal("a morsel's pipeline"));
    };
    let mut g = Grouping::new(group, aggs);
    let cx = Cx { ctx, outer: None };
    let mut spilling = None;
    let mut base = 0;
    match spend {
        Some(spend) => {
            g.limit = usize::MAX;
            let mut held = 0;
            let fed = feed(&mut g, &mut pipe, cx, &mut |g| {
                if spend.grown(held, g.bytes) {
                    spend.release(g.bytes);
                    held = 0;
                    flush(g, &mut spilling, &mut base, &morsel_of)?;
                } else {
                    held = g.bytes;
                }
                Ok(())
            });
            spend.release(held);
            fed?;
            if !spend.keep(&spend.kept, g.bytes) {
                flush(&mut g, &mut spilling, &mut base, &morsel_of)?;
            }
        }
        None => feed(&mut g, &mut pipe, cx, &mut |_| Ok(()))?,
    }
    if g.parts.is_some() {
        return Ok(Part::TooLarge);
    }
    if let Some(at) = ordered {
        return edges(g, aggs.len(), at);
    }
    let buckets = buckets(&mut g, parts, base, &morsel_of)?;
    let spilled = match spilling {
        Some(w) => Some(Spilled {
            run: w.run.seal()?,
            flushes: w.flushes,
        }),
        None => None,
    };
    Ok(Part::Groups { buckets, spilled })
}

/// A worker's groups across the morsels it takes ([`Morsels::run_kept`]),
/// when every aggregate's result is the same in any order of its rows
/// ([`crate::ext::Aggregate::order_free`]): a group recurring in every
/// morsel is one partial state a worker, not one a morsel (TPC-H Q13's
/// customers, ClickBench Q36's pages).  Each group keeps a single pass's
/// rank — the morsel it was first seen in, and its place among the groups
/// first seen there: a worker takes its morsels in increasing order, so
/// the first of them to hold a group is the first it sees it in, and the
/// merge takes a group's least rank.
struct Kept<'a> {
    g: Grouping<'a>,
    spilling: Option<Spilling>,
    /// The place the table's first group has (those before it written or
    /// handed over).
    base: u32,
    /// What of the workers' `work_mem` share the groups hold.
    held: usize,
    /// The place each morsel's new groups start at, with the morsel.
    starts: Vec<(u32, u32)>,
    /// Whether the groups recur, once a second morsel has told.
    recur: Option<bool>,
}

impl<'a> Kept<'a> {
    fn new(group: &'a [Expr], aggs: &'a [Agg]) -> Self {
        let mut g = Grouping::new(group, aggs);
        g.limit = usize::MAX;
        Kept {
            g,
            spilling: None,
            base: 0,
            held: 0,
            starts: Vec::new(),
            recur: None,
        }
    }

    /// The morsel a group's place belongs to.
    fn morsel_of(starts: &[(u32, u32)], place: u32) -> u32 {
        let at = starts.partition_point(|&(p, _)| p <= place);
        starts[at.saturating_sub(1)].1
    }

    /// Morsel `m`'s rows (its key range `range`) into the groups.  The
    /// worker's second morsel tells whether the groups recur: not if it
    /// brought new groups half as many as there were (ClickBench Q32's, a
    /// key a row).  Then the groups are handed over as a part after each
    /// morsel, the table started anew — as a morsel's own table would —:
    /// a table kept for groups that do not recur only grows past the
    /// caches and the worker's share of `work_mem`.
    fn feed(
        &mut self,
        input: &Plan,
        ctx: &Ctx,
        (m, range): (u32, KeyRange),
        spend: &Spend,
    ) -> Result<Option<Part>> {
        let Some(mut pipe) = Pipeline::new(input, input, ctx, None, false, Some(range))? else {
            return Err(Error::internal("a morsel's pipeline"));
        };
        let Kept {
            g,
            spilling,
            base,
            held,
            starts,
            recur,
        } = self;
        let had = g.seen.len();
        starts.push((*base + had as u32, m));
        let cx = Cx { ctx, outer: None };
        let mut flushed = false;
        feed(g, &mut pipe, cx, &mut |g| {
            if spend.grown(*held, g.bytes) {
                spend.release(g.bytes);
                *held = 0;
                flushed = true;
                flush(g, spilling, base, &|p| Kept::morsel_of(starts, p))?;
            } else {
                *held = g.bytes;
            }
            Ok(())
        })?;
        if flushed {
            // The table was emptied on the way: no word on recurrence.
            return Ok(None);
        }
        if had > 0 && recur.is_none() {
            *recur = Some((g.seen.len() - had) * 2 < had);
        }
        if *recur != Some(false) {
            return Ok(None);
        }
        // Handed over, as a morsel's groups were.
        spend.release(*held);
        *held = 0;
        let morsel_of = |p| Kept::morsel_of(starts, p);
        if !spend.keep(&spend.kept, g.bytes) {
            flush(g, spilling, base, &morsel_of)?;
            return Ok(None);
        }
        let n = g.seen.len() as u32;
        let buckets = buckets(g, MERGE_PARTS, *base, &morsel_of)?;
        *base += n;
        *g = Grouping::new(g.group, g.aggs);
        g.limit = usize::MAX;
        Ok(Some(Part::Groups {
            buckets,
            spilled: None,
        }))
    }

    /// The groups as a part (the first morsel the worker took, to choose
    /// between workers' errors).
    fn end(self, spend: &Spend) -> (u32, Result<Part>) {
        let first = self.starts.first().map_or(u32::MAX, |&(_, m)| m);
        let Kept {
            mut g,
            mut spilling,
            mut base,
            held,
            starts,
            ..
        } = self;
        let part = (|| {
            spend.release(held);
            let morsel_of = |p| Kept::morsel_of(&starts, p);
            if !spend.keep(&spend.kept, g.bytes) {
                flush(&mut g, &mut spilling, &mut base, &morsel_of)?;
            }
            if g.parts.is_some() {
                return Ok(Part::TooLarge);
            }
            let buckets = buckets(&mut g, MERGE_PARTS, base, &morsel_of)?;
            let spilled = match spilling {
                Some(w) => Some(Spilled {
                    run: w.run.seal()?,
                    flushes: w.flushes,
                }),
                None => None,
            };
            Ok(Part::Groups { buckets, spilled })
        })();
        (first, part)
    }
}

/// Morsels per worker of a pipeline whose output becomes rows: small, as a
/// round holds a morsel's rows per worker.
const ROW_MORSELS_PER_WORKER: usize = 16;
/// A chain without a filter a `LIMIT` stops within this many rows is read
/// on one thread: a parallel run's first morsels would read more.
const SERIAL_ROWS: usize = 1 << 16;

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
    rows: Held,
    index: JoinIndex,
}

/// A join table's rows: as they came (rows already in memory), or a column
/// at a time (gathered from batches: no allocation per row, and a fraction
/// of a row's memory for numbers).
enum Held {
    Rows(Vec<Row>),
    Columns(crate::columns::Columns),
}

impl Held {
    fn len(&self) -> usize {
        match self {
            Held::Rows(r) => r.len(),
            Held::Columns(c) => c.len(),
        }
    }

    /// Row `r`'s values appended to `out`.
    fn extend_row(&self, r: usize, out: &mut Row) {
        match self {
            Held::Rows(rows) => out.extend_from_slice(&rows[r]),
            Held::Columns(c) => c.extend_row(r, out),
        }
    }

    /// Column `c` of rows `rows` (`None`: a `NULL`), as a vector.
    fn column(&self, c: usize, rows: &[Option<u32>]) -> Vector {
        match self {
            Held::Columns(cols) => cols.gather(c, rows),
            Held::Rows(held) => Vector::typed(
                rows.iter()
                    .map(|r| r.map_or(Value::Null, |i| held[i as usize][c].clone()))
                    .collect(),
            ),
        }
    }

    /// The rows, in order.
    fn into_rows(self) -> Vec<Row> {
        match self {
            Held::Rows(r) => r,
            Held::Columns(c) => (0..c.len())
                .map(|r| {
                    let mut row = Vec::new();
                    c.extend_row(r, &mut row);
                    row
                })
                .collect(),
        }
    }
}

/// A join table's rows by key, found as a grouping finds its groups
/// (`crate::keytable`: one key table for both).
enum JoinIndex {
    /// One key, an integer in every right row that has one: a direct index
    /// over a narrow range, hashed beyond (`IntKeys`); each group's key.
    Int(crate::keytable::IntKeys, Vec<i64>, Runs),
    /// Two keys, integers in every right row that has them.
    Pair(crate::fasthash::FastMap<(i64, i64), u32>, Runs),
    /// Anything else: by the keys' hashes, a probe's key compared from the
    /// batch's vectors as they are (no key built a probe); each group's
    /// key.
    Any(crate::keytable::KeyTable, Vec<Vec<Value>>, Runs),
}

/// Row numbers by group: each group's, ascending, side by side in one
/// array — no allocation per key, to make or to free.
struct Runs {
    /// Where each group starts in `rows` (and, one more, where it ends).
    starts: Vec<u32>,
    rows: Vec<u32>,
}

impl Runs {
    /// The rows of `keys` (one a row; `None` in no group), each key's group
    /// found or made by `group` (given the next group's number).
    fn of<K>(
        keys: impl ExactSizeIterator<Item = Option<K>>,
        mut group: impl FnMut(K, u32) -> u32,
    ) -> Self {
        const NONE: u32 = u32::MAX;
        let mut of = Vec::with_capacity(keys.len());
        let mut counts: Vec<u32> = Vec::new();
        for k in keys {
            let Some(k) = k else {
                of.push(NONE);
                continue;
            };
            let next = counts.len() as u32;
            let g = group(k, next);
            if g == next {
                counts.push(0);
            }
            counts[g as usize] += 1;
            of.push(g);
        }
        let mut starts = Vec::with_capacity(counts.len() + 1);
        let mut at = 0u32;
        for c in &counts {
            starts.push(at);
            at += c;
        }
        starts.push(at);
        let mut fill = starts[..counts.len()].to_vec();
        let mut rows = vec![0u32; at as usize];
        for (i, g) in of.into_iter().enumerate() {
            if g != NONE {
                rows[fill[g as usize] as usize] = i as u32;
                fill[g as usize] += 1;
            }
        }
        Runs { starts, rows }
    }

    /// Group `g`'s rows, ascending.
    fn get(&self, g: u32) -> &[u32] {
        let g = g as usize;
        &self.rows[self.starts[g] as usize..self.starts[g + 1] as usize]
    }
}

impl JoinTable {
    /// `rows` by their `keys` (a row with a NULL key left out: NULL equals
    /// nothing).
    fn build(rows: Vec<Row>, keys: &[Expr], ctx: &Ctx, outer: Option<&Env>) -> Result<Self> {
        let mut values = crate::columns::Columns::new(keys.len());
        for r in &rows {
            let e = env(r, ctx, outer);
            values.push_row(keys.iter().map(|k| k.eval(&e)).collect::<Result<_>>()?);
        }
        Ok(Self::of(Held::Rows(rows), values))
    }

    /// The table of the pipeline `right` (a hash join's build side), made
    /// on worker threads: morsels of its scan in key order, each collected
    /// with its keys' values by a worker, joined in order — the rows, each
    /// key's order and the error a single pass's (a scan's error before a
    /// key's).  `None` when it cannot run so ([`Morsels::new`]) or the rows
    /// pass work_mem (the caller stashes them, spilling).
    pub(crate) fn parallel(
        right: &Plan,
        keys: &[Expr],
        ctx: &Ctx,
        outer: Option<&Env>,
    ) -> Result<Option<Self>> {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        let exprs: Vec<&Expr> = keys.iter().collect();
        let Some(work) = Morsels::new(right, &exprs, ctx, outer, ROW_MORSELS_PER_WORKER)? else {
            return Ok(None);
        };
        PARALLEL_RUNS.fetch_add(1, Relaxed);
        let limit = crate::budget::work_mem();
        let bytes = AtomicUsize::new(0);
        let width = right.width();
        type Part = Option<(crate::columns::Columns, Result<crate::columns::Columns>)>;
        let results = work.run(0..work.ranges.len(), true, &|wctx, range| -> Result<Part> {
            let Some(mut pipe) = Pipeline::new(right, right, wctx, None, false, Some(range))?
            else {
                return Err(Error::internal("a morsel's pipeline"));
            };
            let cx = Cx {
                ctx: wctx,
                outer: None,
            };
            let mut rows = crate::columns::Columns::new(width);
            let mut values = crate::columns::Columns::new(keys.len());
            // The first key error, kept until the scan is done: a scan's
            // error comes first.
            let mut key_err = None;
            let mut size;
            loop {
                match pipe.next()? {
                    None => break,
                    Some(Out::Batch(b)) => {
                        if key_err.is_none() {
                            match keys
                                .iter()
                                .map(|k| eval(k, &b, &b.sel, cx))
                                .collect::<Result<Vec<_>>>()
                            {
                                Ok(vs) => values.push_vectors(&vs, &b.sel),
                                Err(e) => key_err = Some(e),
                            }
                        }
                        rows.push_batch(&b);
                    }
                    Some(Out::Rows(batch, err)) => {
                        for r in batch {
                            if key_err.is_none() {
                                let e = env(&r, wctx, None);
                                match keys.iter().map(|k| k.eval(&e)).collect::<Result<_>>() {
                                    Ok(k) => values.push_row(k),
                                    Err(e) => key_err = Some(e),
                                }
                            }
                            rows.push_row(r);
                        }
                        if let Some(e) = err {
                            return Err(e);
                        }
                    }
                }
                // The table's memory: the rows and their keys' values.
                size = rows.bytes() + values.bytes();
                if bytes.load(Relaxed) + size > limit {
                    return Ok(None);
                }
            }
            let size = rows.bytes() + values.bytes();
            if bytes.fetch_add(size, Relaxed) + size > limit {
                return Ok(None);
            }
            Ok(Some((rows, key_err.map_or(Ok(values), Err))))
        });
        let mut parts = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Some(Ok(Some(part))) => parts.push(part),
                Some(Ok(None)) => return Ok(None), // past work_mem
                Some(Err(e)) => return Err(e),
                None => return Err(Error::internal("a morsel not run")),
            }
        }
        let mut rows = crate::columns::Columns::new(width);
        let mut values = crate::columns::Columns::new(keys.len());
        for (r, v) in parts {
            rows.append(r);
            values.append(v?);
        }
        Ok(Some(Self::of(Held::Columns(rows), values)))
    }

    /// The integer values of key `j` (`None`: not all integers) — those
    /// of the rows it holds (a row with a `NULL` key is in no group).
    pub(crate) fn key_ints(&self, j: usize) -> Option<Vec<i64>> {
        match &self.index {
            JoinIndex::Int(_, keys, _) if j == 0 => Some(keys.clone()),
            JoinIndex::Int(..) => None,
            JoinIndex::Pair(m, _) => match j {
                0 => Some(m.keys().map(|k| k.0).collect()),
                1 => Some(m.keys().map(|k| k.1).collect()),
                _ => None,
            },
            JoinIndex::Any(_, keys, _) => keys
                .iter()
                .map(|k| match k.get(j) {
                    Some(Value::Int(x)) => Some(*x),
                    _ => None,
                })
                .collect(),
        }
    }

    /// A hash join's build side gathered a column at a time, its keys then
    /// evaluated (after every row, as when stashed: a row's error before a
    /// key's) — or, once its columns pass work_mem, its rows stashed as
    /// [`crate::exec`] stashes them (spilling).
    pub(crate) fn gather(
        mut rows: Rows,
        width: usize,
        keys: &[Expr],
        ctx: &Ctx,
        outer: Option<&Env>,
    ) -> Result<Gathered> {
        let limit = crate::budget::work_mem();
        let mut held = crate::columns::Columns::new(width);
        while let Some(r) = rows.next() {
            held.push_row(r?);
            if held.pushed_bytes() > limit {
                let mut stash = crate::spill::Stash::new();
                for i in 0..held.len() {
                    let mut row = Vec::with_capacity(width);
                    held.extend_row(i, &mut row);
                    stash.push(row)?;
                }
                drop(held);
                for r in rows {
                    stash.push(r?)?;
                }
                return Ok(Gathered::Stored(stash.seal()?));
            }
        }
        let columns: Option<Vec<usize>> = keys
            .iter()
            .map(|k| match k {
                Expr::Column(c) => Some(*c),
                _ => None,
            })
            .collect();
        let values = match columns {
            Some(cols) => held.project(&cols),
            None => {
                let mut values = crate::columns::Columns::new(keys.len());
                let mut row = Vec::with_capacity(width);
                for i in 0..held.len() {
                    row.clear();
                    held.extend_row(i, &mut row);
                    let e = env(&row, ctx, outer);
                    values.push_row(keys.iter().map(|k| k.eval(&e)).collect::<Result<_>>()?);
                }
                values
            }
        };
        Ok(Gathered::Table(Self::of(Held::Columns(held), values)))
    }

    /// How many rows it holds.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Its rows, in order, as rows.
    pub(crate) fn rows_in_order(&self) -> Vec<Row> {
        (0..self.rows.len())
            .map(|r| {
                let mut row = Vec::new();
                self.rows.extend_row(r, &mut row);
                row
            })
            .collect()
    }

    /// `rows` by their keys' values (`values`: a row's keys a row).
    fn of(rows: Held, values: crate::columns::Columns) -> Self {
        let index = match (values.ints(), values.int_pairs()) {
            (Some(ints), _) => {
                let (mut table, mut keys) = (crate::keytable::IntKeys::default(), Vec::new());
                let runs = Runs::of(ints, |x, next| {
                    table.get(x).unwrap_or_else(|| {
                        table.insert(x, next);
                        keys.push(x);
                        next
                    })
                });
                JoinIndex::Int(table, keys, runs)
            }
            (None, Some(pairs)) => {
                let mut map: crate::fasthash::FastMap<(i64, i64), u32> = Default::default();
                map.reserve(pairs.len());
                let runs = Runs::of(pairs, |k, next| *map.entry(k).or_insert(next));
                JoinIndex::Pair(map, runs)
            }
            (None, None) => {
                let (mut table, mut keys) = (crate::keytable::KeyTable::default(), Vec::new());
                let all = (0..values.len()).map(|r| {
                    let mut k = Vec::new();
                    values.extend_row(r, &mut k);
                    (!k.iter().any(Value::is_null)).then_some(k)
                });
                let runs = Runs::of(all, |k: Vec<Value>, next| {
                    let h = KeyRef::Values(&k).hash(&table.state);
                    match table.find(h, |g| keys[g as usize] == k) {
                        Some(g) => g,
                        None => {
                            table.insert(h);
                            keys.push(k);
                            next
                        }
                    }
                });
                JoinIndex::Any(table, keys, runs)
            }
        };
        JoinTable { rows, index }
    }
}

/// A hash join's build side gathered ([`JoinTable::gather`]).
pub(crate) enum Gathered {
    Table(JoinTable),
    /// Past work_mem: its rows, stashed.
    Stored(crate::spill::Stored),
}

/// How a hash join's left rows probe its table.
pub(crate) struct Probe<'a> {
    table: std::sync::Arc<JoinTable>,
    left_keys: &'a [Expr],
    on: Option<&'a Expr>,
    kind: crate::plan::JoinType,
    right_width: usize,
    /// Whether `on` must be checked on each candidate (not when it is just
    /// the integer keys' equality, which the table already decided).
    check: bool,
}

impl<'a> Probe<'a> {
    /// The probe of `table` by a join's left rows (`left_width` wide).
    fn new(
        table: std::sync::Arc<JoinTable>,
        left_width: usize,
        left_keys: &'a [Expr],
        right_keys: &'a [Expr],
        kind: JoinType,
        on: Option<&'a Expr>,
        right_width: usize,
    ) -> Self {
        let check = on.is_some()
            && !(keys_only(on, left_keys, right_keys, left_width)
                && matches!(table.index, JoinIndex::Int(..) | JoinIndex::Pair(..)));
        Probe {
            table,
            left_keys,
            on,
            kind,
            right_width,
            check,
        }
    }

    /// A batch of left rows probed, as a batch (a pipeline's step): the
    /// rows, and their order, of [`Probe::batch`] — each left row's
    /// matches in the table's order, a left join's unmatched row with
    /// `NULL`s, a semi or anti join's rows as they were.  `on` beyond the
    /// keys is evaluated over every candidate pair at once (an error there
    /// sends the batch a row at a time, as a pipeline's errors do).
    fn apply(&self, b: Batch, cx: Cx) -> Result<Batch> {
        let keys = self
            .left_keys
            .iter()
            .map(|k| eval(k, &b, &b.sel, cx))
            .collect::<Result<Vec<_>>>()?;
        let found: Vec<(u32, &[u32])> = b
            .sel
            .iter()
            .map(|i| (i as u32, self.candidates(KeyRef::Batch(&keys, i))))
            .collect();
        let semi = self.kind == JoinType::Semi;
        let existence = matches!(self.kind, JoinType::Semi | JoinType::Anti);
        if !self.check {
            if existence {
                let rows = found
                    .iter()
                    .filter(|(_, c)| c.is_empty() != semi)
                    .map(|&(i, _)| i)
                    .collect();
                return Ok(Batch {
                    sel: Sel::Rows(rows),
                    ..b
                });
            }
            let outer = self.kind == JoinType::Left;
            let (mut l, mut r) = (Vec::new(), Vec::new());
            for &(i, c) in &found {
                if c.is_empty() && outer {
                    l.push(i);
                    r.push(None);
                }
                for &x in c {
                    l.push(i);
                    r.push(Some(x));
                }
            }
            return Ok(self.joined(&b, l, &r));
        }
        // Every candidate pair, `on` over them all.
        let (mut l, mut r) = (Vec::new(), Vec::new());
        for &(i, c) in &found {
            for &x in c {
                l.push(i);
                r.push(Some(x));
            }
        }
        let on = self.on.expect("checked: there is an `on`");
        let pairs = filter(on, self.joined(&b, l.clone(), &r), cx)?;
        if self.kind == JoinType::Inner {
            return Ok(pairs);
        }
        let mut held = vec![false; l.len()];
        for p in pairs.sel.iter() {
            held[p] = true;
        }
        // Each left row's pairs are consecutive.
        let mut at = 0;
        if existence {
            let mut rows = Vec::new();
            for &(i, c) in &found {
                let matched = held[at..at + c.len()].contains(&true);
                at += c.len();
                if matched == semi {
                    rows.push(i);
                }
            }
            return Ok(Batch {
                sel: Sel::Rows(rows),
                ..b
            });
        }
        let (mut l2, mut r2) = (Vec::new(), Vec::new());
        for &(i, c) in &found {
            let mut matched = false;
            for k in at..at + c.len() {
                if held[k] {
                    matched = true;
                    l2.push(i);
                    r2.push(r[k]);
                }
            }
            at += c.len();
            if !matched {
                l2.push(i);
                r2.push(None);
            }
        }
        Ok(self.joined(&b, l2, &r2))
    }

    /// The pairs of `b`'s rows `left` and the table's rows `right` (`None`:
    /// `NULL`s), as a batch.
    fn joined(&self, b: &Batch, left: Vec<u32>, right: &[Option<u32>]) -> Batch {
        let n = left.len();
        let picked = Sel::Rows(left);
        let mut cols: Vec<Rc<Vector>> = b.cols.iter().map(|c| Rc::new(c.gather(&picked))).collect();
        cols.extend((0..self.right_width).map(|c| Rc::new(self.table.rows.column(c, right))));
        Batch {
            len: n,
            cols,
            sel: Sel::All(n),
        }
    }

    /// The right rows a left key matches.
    fn candidates(&self, key: KeyRef) -> &[u32] {
        let found = match &self.table.index {
            JoinIndex::Int(t, _, runs) => key.int(0).and_then(|x| t.get(x)).map(|g| runs.get(g)),
            JoinIndex::Pair(m, runs) => match (key.int(0), key.int(1)) {
                (Some(a), Some(b)) => m.get(&(a, b)).map(|&g| runs.get(g)),
                _ => None,
            },
            JoinIndex::Any(t, keys, runs) => {
                if key.has_null() {
                    None
                } else {
                    t.find(key.hash(&t.state), |g| key.eq(&keys[g as usize]))
                        .map(|g| runs.get(g))
                }
            }
        };
        found.unwrap_or(&[])
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
            if self.check {
                let mut combined = left();
                self.table.rows.extend_row(r as usize, &mut combined);
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
                    self.table.rows.extend_row(r as usize, &mut combined);
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
        // The whole batch's lookups first, then its output: lookups that
        // depend on nothing but their keys, back to back, overlap their
        // cache misses (what prefetching in a hash probe is for).
        let found: Vec<&[u32]> = b
            .sel
            .iter()
            .map(|i| self.candidates(KeyRef::Batch(&keys, i)))
            .collect();
        for (i, c) in b.sel.iter().zip(found) {
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
            let c = self.candidates(KeyRef::Values(&k));
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

/// Whether `plan` can be read a batch at a time as it is (a pipeline with
/// no joins: a hash join's build side).
pub(crate) fn probes(plan: &Plan) -> bool {
    crate::budget::batch_size() > 0 && chain(plan, None).is_some()
}

/// Whether `left` can be a vectorized hash join's probe side: a pipeline,
/// once the tables of the hash joins in it are made ([`prepare`]).
pub(crate) fn probe_side(left: &Plan) -> bool {
    crate::budget::batch_size() > 0 && shaped(left)
}

/// A hash join of the pipeline `left` with the right rows `rows` (in
/// memory), vectorized: the right rows hashed once (an integer key as an
/// integer), the left side's keys evaluated a batch at a time and probed —
/// on worker threads, morsels of the left scan in key order, when it is
/// large enough; the rows, their order and errors those of the row-at-a-
/// time join.  `Err(rows)` when `left` is not a pipeline after all (a join
/// in it hashed a side past work_mem): the right rows back.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hash_join<'a>(
    left: &'a Plan,
    rows: Vec<Row>,
    left_keys: &'a [Expr],
    right_keys: &'a [Expr],
    kind: JoinType,
    on: Option<&'a Expr>,
    right_width: usize,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<std::result::Result<Rows<'a>, Vec<Row>>> {
    let table = JoinTable::build(rows, right_keys, ctx, outer)?;
    hash_join_table(
        left,
        std::sync::Arc::new(table),
        left_keys,
        right_keys,
        kind,
        on,
        right_width,
        ctx,
        outer,
    )
}

/// [`hash_join`] with its table made.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hash_join_table<'a>(
    left: &'a Plan,
    table: std::sync::Arc<JoinTable>,
    left_keys: &'a [Expr],
    right_keys: &'a [Expr],
    kind: JoinType,
    on: Option<&'a Expr>,
    right_width: usize,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<std::result::Result<Rows<'a>, Vec<Row>>> {
    let tables = prepare(left, ctx, outer)?;
    if chain(left, Some(&ctx.join_tables.borrow())).is_none() {
        tables.keep();
        return Ok(Err(table.rows_in_order()));
    }
    let probe = Probe::new(
        table,
        left.width(),
        left_keys,
        right_keys,
        kind,
        on,
        right_width,
    );
    let mut exprs: Vec<&Expr> = left_keys.iter().collect();
    exprs.extend(on);
    if let Some(work) = Morsels::new(left, &exprs, ctx, outer, ROW_MORSELS_PER_WORKER)? {
        PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Ok(Box::new(ParallelJoin {
            work,
            plan: left,
            probe,
            next: 0,
            rows: std::collections::VecDeque::new(),
            err: None,
            done: false,
        })));
    }
    let Some(pipe) = Pipeline::new(left, left, ctx, outer, false, None)? else {
        return Err(Error::internal("a hash join's probe pipeline"));
    };
    Ok(Ok(Box::new(JoinRows {
        pipe: Some(pipe),
        left,
        probe,
        cx: Cx { ctx, outer },
        rows: std::collections::VecDeque::new(),
        err: None,
        measured: (Duration::ZERO, 0),
    })))
}

/// The hash joins' tables a pipeline probes, by the join's address
/// ([`prepare`]).
pub(crate) type Tables = HashMap<usize, Built>;

/// A hash join's build side, made before the pipeline probing it runs.
pub(crate) enum Built {
    Table(std::sync::Arc<JoinTable>),
    /// Past work_mem: its rows, stashed, for [`crate::exec`]'s join (the
    /// pipeline above it does not run as one).
    Stored(crate::spill::Stored),
}

/// The tables a pipeline's workers probe.
type Shared = Vec<(usize, std::sync::Arc<JoinTable>)>;

fn snapshot(tables: &Tables) -> Shared {
    tables
        .iter()
        .filter_map(|(j, b)| match b {
            Built::Table(t) => Some((*j, t.clone())),
            Built::Stored(_) => None,
        })
        .collect()
}

fn address(plan: &Plan) -> usize {
    plan as *const Plan as usize
}

/// Whether a hash join of `kind` probes in a pipeline.
fn probing(kind: JoinType) -> bool {
    matches!(
        kind,
        JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti
    )
}

/// Whether `plan` is a chain once its hash joins' tables are made: a scan,
/// filters and projections, and hash joins on their right sides probed by
/// their left.
fn shaped(plan: &Plan) -> bool {
    match plan {
        Plan::Scan { access, .. } => access.secondary().is_none(),
        Plan::Filter { input, .. } | Plan::Secure { input, .. } | Plan::Project { input, .. } => {
            shaped(input)
        }
        Plan::Join {
            left,
            kind,
            algo: JoinAlgo::Hash {
                build_left: false, ..
            },
            ..
        } => probing(*kind) && shaped(left),
        _ => false,
    }
}

/// The hash joins in the chain `plan` with their tables made — each right
/// side run as [`crate::exec`]'s join runs it (on the workers when it is a
/// pipeline, else gathered), outermost first as it runs them, and its key
/// filter handed to the scans below — so a pipeline can probe them (HyPer's
/// morsel-driven pipelines, the probe a step of the pipeline: Leis et al.,
/// SIGMOD 2014).  A right side past work_mem is stashed, and the joins
/// below it are left alone: the pipeline does not run as one, and
/// `crate::exec`'s join takes the stashed rows (and the tables made above
/// them) rather than running the right side again.  Not with an enclosing
/// row (a correlated subquery's plan runs again for every row), nor when
/// vectorized execution is off.  The guard returned drops the tables once
/// the pipelines are made (they keep what they probe); kept
/// ([`Prepared::keep`]) when the caller does not run the pipeline, for the
/// row-at-a-time executor running it instead.
pub(crate) fn prepare<'a>(
    plan: &'a Plan,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Prepared<'a>> {
    let mut made = Prepared {
        ctx,
        joins: Vec::new(),
        keep: false,
    };
    if crate::budget::batch_size() > 0 && outer.is_none() && shaped(plan) {
        make(plan, ctx, &mut made.joins)?;
    }
    Ok(made)
}

fn make<'a>(plan: &'a Plan, ctx: &'a Ctx<'a>, joins: &mut Vec<usize>) -> Result<()> {
    match plan {
        Plan::Filter { input, .. } | Plan::Secure { input, .. } | Plan::Project { input, .. } => {
            make(input, ctx, joins)
        }
        Plan::Join {
            left,
            right,
            kind,
            algo:
                JoinAlgo::Hash {
                    left_keys,
                    right_keys,
                    build_left: false,
                    adaptive,
                },
            ..
        } => {
            let at = address(plan);
            joins.push(at);
            // Made by a caller that then did not run the pipeline: as made.
            let made = ctx
                .join_tables
                .borrow()
                .get(&at)
                .map(|b| matches!(b, Built::Table(_)));
            match made {
                Some(true) => return make(left, ctx, joins),
                Some(false) => return Ok(()),
                None => {}
            }
            let mut table = None;
            if probes(right) {
                table = JoinTable::parallel(right, right_keys, ctx, None)?;
            }
            let table = match table {
                Some(t) => t,
                None => match JoinTable::gather(
                    crate::exec::build(right, ctx, None)?,
                    right.width(),
                    right_keys,
                    ctx,
                    None,
                )? {
                    Gathered::Table(t) => t,
                    Gathered::Stored(rows) => {
                        ctx.join_tables.borrow_mut().insert(at, Built::Stored(rows));
                        return Ok(());
                    }
                },
            };
            // An adaptive join's few right rows look the left side up
            // instead: `crate::exec`'s join takes them.
            if let Some(a) = adaptive
                && table.len() <= a.below
            {
                let rows = crate::spill::Stored::Memory(table.rows_in_order());
                ctx.join_tables.borrow_mut().insert(at, Built::Stored(rows));
                return Ok(());
            }
            crate::keyfilter::register(ctx, left, right, *kind, left_keys, &|j| table.key_ints(j));
            ctx.join_tables
                .borrow_mut()
                .insert(at, Built::Table(std::sync::Arc::new(table)));
            make(left, ctx, joins)
        }
        _ => Ok(()),
    }
}

/// The join `node`'s build side made by a [`prepare`] whose pipeline did
/// not run, taken.
pub(crate) fn take_built(ctx: &Ctx, node: &Plan) -> Option<Built> {
    ctx.join_tables.borrow_mut().remove(&address(node))
}

/// The tables [`prepare`] made: dropped with it, unless kept.
pub(crate) struct Prepared<'a> {
    ctx: &'a Ctx<'a>,
    joins: Vec<usize>,
    keep: bool,
}

impl Prepared<'_> {
    /// The tables kept for the row-at-a-time executor (the caller does not
    /// run the pipeline).
    pub(crate) fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        if !self.keep && !self.joins.is_empty() {
            let mut tables = self.ctx.join_tables.borrow_mut();
            for j in &self.joins {
                tables.remove(j);
            }
        }
    }
}

/// Whether a join's `on` is only its keys' equality (an integer table
/// decides it).
fn keys_only(on: Option<&Expr>, left_keys: &[Expr], right_keys: &[Expr], lw: usize) -> bool {
    on.is_none_or(|on| {
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
    })
}

/// A semi or anti join hashed on its left rows (`JoinAlgo::Hash`'s
/// `build_left`): the left rows' keys in a table, the pipeline `right`
/// probing it a batch at a time — on worker threads when it is large —
/// marking the left rows it matches; then the left rows in order, those
/// marked (semi) or not (anti): the rows and order of the join that hashes
/// its right side.
#[allow(clippy::too_many_arguments)]
pub(crate) fn existence_by_left<'a>(
    left: Vec<Row>,
    left_width: usize,
    right: &'a Plan,
    left_keys: &'a [Expr],
    right_keys: &'a [Expr],
    kind: crate::plan::JoinType,
    on: Option<&'a Expr>,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    let table = JoinTable::build(left, left_keys, ctx, outer)?;
    // (`keys_only` reads its first keys as the left row's.)
    let check = on.is_some()
        && !(keys_only(on, left_keys, right_keys, left_width)
            && matches!(table.index, JoinIndex::Int(..) | JoinIndex::Pair(..)));
    let probe = Probe {
        table: std::sync::Arc::new(table),
        left_keys: right_keys,
        on,
        kind,
        right_width: right.width(),
        check,
    };
    let marks: Vec<AtomicBool> = (0..probe.table.rows.len())
        .map(|_| AtomicBool::new(false))
        .collect();
    let mark = |pipe: &mut Pipeline, cx: Cx| -> Result<()> {
        let one = |key: KeyRef, row: &dyn Fn() -> Row| -> Result<()> {
            for &c in probe.candidates(key) {
                let c = c as usize;
                if marks[c].load(Relaxed) {
                    continue;
                }
                if probe.check {
                    let mut combined = Vec::new();
                    probe.table.rows.extend_row(c, &mut combined);
                    combined.extend(row());
                    let holds =
                        on.map_or(Ok(true), |on| on.holds(&env(&combined, cx.ctx, cx.outer)))?;
                    if !holds {
                        continue;
                    }
                }
                marks[c].store(true, Relaxed);
            }
            Ok(())
        };
        loop {
            match pipe.next()? {
                None => return Ok(()),
                Some(Out::Batch(b)) => {
                    let keys = right_keys
                        .iter()
                        .map(|k| eval(k, &b, &b.sel, cx))
                        .collect::<Result<Vec<_>>>()?;
                    for i in b.sel.iter() {
                        one(KeyRef::Batch(&keys, i), &|| b.row(i))?;
                    }
                }
                Some(Out::Rows(rows, err)) => {
                    for row in rows {
                        let e = env(&row, cx.ctx, cx.outer);
                        let k = right_keys
                            .iter()
                            .map(|k| k.eval(&e))
                            .collect::<Result<Vec<_>>>()?;
                        one(KeyRef::Values(&k), &|| row.clone())?;
                    }
                    if let Some(e) = err {
                        return Err(e);
                    }
                }
            }
        }
    };
    let mut exprs: Vec<&Expr> = right_keys.iter().collect();
    exprs.extend(on);
    if let Some(work) = Morsels::new(right, &exprs, ctx, outer, ROW_MORSELS_PER_WORKER)? {
        PARALLEL_RUNS.fetch_add(1, Relaxed);
        let results = work.run(0..work.ranges.len(), true, &|wctx, range| {
            let Some(mut pipe) = Pipeline::new(right, right, wctx, None, false, Some(range))?
            else {
                return Err(Error::internal("a morsel's pipeline"));
            };
            mark(
                &mut pipe,
                Cx {
                    ctx: wctx,
                    outer: None,
                },
            )
        });
        let mut skipped = false;
        for r in results {
            match r {
                Some(Ok(())) => {}
                Some(Err(e)) => return Err(e),
                None => skipped = true,
            }
        }
        if skipped {
            return Err(Error::internal("a morsel not run"));
        }
    } else {
        let Some(mut pipe) = Pipeline::new(right, right, ctx, outer, false, None)? else {
            return Err(Error::internal("a semi join's probe pipeline"));
        };
        let profile = ctx.profile;
        let started = profile.and_then(|p| p.clock());
        mark(&mut pipe, Cx { ctx, outer })?;
        if let Some(p) = profile
            && let (Some(t0), Some(t1)) = (started, p.clock())
        {
            p.record(right, t1.saturating_sub(t0), 0, 1);
        }
    }
    let semi = kind == crate::plan::JoinType::Semi;
    let table = std::sync::Arc::try_unwrap(probe.table)
        .map_err(|_| Error::internal("a join table still shared"))?;
    let rows: Vec<Row> = table
        .rows
        .into_rows()
        .into_iter()
        .zip(marks)
        .filter(|(_, m)| m.load(Relaxed) == semi)
        .map(|(r, _)| r)
        .collect();
    Ok(Box::new(rows.into_iter().map(Ok)))
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

/// A morsel's pairs being sorted: all of them, or (with a bound) the
/// least.
enum Sorting {
    All(crate::sorter::Sorter),
    Top(crate::spill::Top),
}

/// A morsel sorted: its parts, or its least pairs.
enum Sorted {
    All(Vec<std::sync::Arc<crate::sorter::Part>>),
    Top(Vec<(Vec<Value>, Row)>),
}

impl Sorting {
    /// A pair, its row built only when kept.
    fn take(&mut self, keys: Vec<Value>, row: impl FnOnce() -> Row) -> Result<()> {
        match self {
            Sorting::Top(t) => {
                if t.admits(&keys) {
                    t.push(keys, row());
                }
                Ok(())
            }
            Sorting::All(s) => s.push(&keys, &row()),
        }
    }
}

/// `ORDER BY keys` over the pipeline `input`, in parallel: each morsel's
/// rows sorted on a worker (their keys evaluated a batch at a time), the
/// sorted morsels merged ([`crate::sorter::merge`]; with a bound,
/// [`crate::spill::merge_sorted`]) — the order of the single pass's stable
/// sort, its first error the same.  With
/// `bound`, a morsel keeps only its first `bound` ([`crate::spill::Top`]),
/// a row built only when its keys put it among them.
pub(crate) fn sorted<'a>(
    input: &'a Plan,
    keys: &'a [crate::plan::SortKey],
    bound: Option<usize>,
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
    let shared: std::sync::Arc<[(bool, bool)]> = flags.clone().into();
    let budget = crate::sorter::Budget::new(crate::budget::workers());
    let results = work.run(0..work.ranges.len(), true, &|wctx, range| {
        let Some(mut pipe) = Pipeline::new(input, input, wctx, None, false, Some(range))? else {
            return Err(Error::internal("a morsel's pipeline"));
        };
        let cx = Cx {
            ctx: wctx,
            outer: None,
        };
        let mut sink = match bound {
            Some(k) => Sorting::Top(crate::spill::Top::new(k, flags.clone().into())),
            None => Sorting::All(crate::sorter::Sorter::new(
                std::sync::Arc::clone(&shared),
                std::sync::Arc::clone(&budget),
            )),
        };
        let by_row = |row: Row, sink: &mut Sorting| -> Result<()> {
            let e = env(&row, wctx, None);
            let k = exprs
                .iter()
                .map(|x| x.eval(&e))
                .collect::<Result<Vec<_>>>()?;
            sink.take(k, || row)
        };
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
                                sink.take(kv.iter().map(|k| k.value(i)).collect(), || b.row(i))?;
                            }
                        }
                        // A row at a time decides.
                        Err(_) => {
                            for i in b.sel.iter() {
                                by_row(b.row(i), &mut sink)?;
                            }
                        }
                    }
                }
                Out::Rows(rows, err) => {
                    for row in rows {
                        by_row(row, &mut sink)?;
                    }
                    if let Some(e) = err {
                        return Err(e);
                    }
                }
            }
        }
        match sink {
            Sorting::Top(t) => Ok(Sorted::Top(t.into_sorted())),
            Sorting::All(s) => Ok(Sorted::All(s.finish()?)),
        }
    });
    let mut tops = Vec::new();
    let mut parts = Vec::new();
    for r in results {
        match r {
            Some(Ok(Sorted::Top(p))) => tops.push(p),
            Some(Ok(Sorted::All(p))) => parts.extend(p),
            Some(Err(e)) => return Err(e),
            None => return Err(Error::internal("a morsel not sorted")),
        }
    }
    PARALLEL_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if bound.is_some() {
        return Ok(Some(crate::spill::merge_sorted(tops, flags)?));
    }
    Ok(Some(crate::sorter::merge(parts, shared)?))
}
