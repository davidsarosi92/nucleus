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
use crate::plan::{Access, Agg, Plan};
use crate::types::{DataType, Row, Value, total_cmp};

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
    /// Any values.
    Values(Vec<Value>),
}

fn is_null_at(nulls: &Option<Vec<bool>>, i: usize) -> bool {
    nulls.as_ref().is_some_and(|n| n[i])
}

impl Vector {
    /// Row `i`'s value.
    pub fn value(&self, i: usize) -> Value {
        match self {
            Vector::Const(v) => v.clone(),
            Vector::Int(d, n) if !is_null_at(n, i) => Value::Int(d[i]),
            Vector::Float(d, n) if !is_null_at(n, i) => Value::Float(d[i]),
            Vector::Bool(d, n) if !is_null_at(n, i) => Value::Bool(d[i]),
            Vector::Values(v) => v[i].clone(),
            _ => Value::Null,
        }
    }

    /// Whether row `i` is `NULL`.
    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Vector::Const(v) => v.is_null(),
            Vector::Int(_, n) | Vector::Float(_, n) | Vector::Bool(_, n) => is_null_at(n, i),
            Vector::Values(v) => v[i].is_null(),
        }
    }

    /// `values` as a typed vector when they are all integers, doubles or
    /// booleans (or `NULL`).
    pub fn typed(values: Vec<Value>) -> Vector {
        let nulls = |values: &[Value]| {
            let n: Vec<bool> = values.iter().map(Value::is_null).collect();
            n.contains(&true).then_some(n)
        };
        match values.iter().find(|v| !v.is_null()) {
            Some(Value::Int(_))
                if values
                    .iter()
                    .all(|v| matches!(v, Value::Int(_) | Value::Null)) =>
            {
                let data = values
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                    .collect();
                Vector::Int(data, nulls(&values))
            }
            Some(Value::Float(_))
                if values
                    .iter()
                    .all(|v| matches!(v, Value::Float(_) | Value::Null)) =>
            {
                let data = values
                    .iter()
                    .map(|v| if let Value::Float(f) = v { *f } else { 0.0 })
                    .collect();
                Vector::Float(data, nulls(&values))
            }
            Some(Value::Bool(_))
                if values
                    .iter()
                    .all(|v| matches!(v, Value::Bool(_) | Value::Null)) =>
            {
                let data = values
                    .iter()
                    .map(|v| matches!(v, Value::Bool(true)))
                    .collect();
                Vector::Bool(data, nulls(&values))
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

/// One operand of a typed kernel: a vector or a constant.
#[derive(Clone, Copy)]
enum Side<'a, T> {
    Vec(&'a [T], Option<&'a [bool]>),
    Const(T),
}

impl<T: Copy> Side<'_, T> {
    fn at(&self, i: usize) -> Option<T> {
        match self {
            Side::Vec(d, None) => Some(d[i]),
            Side::Vec(d, Some(n)) => (!n[i]).then(|| d[i]),
            Side::Const(c) => Some(*c),
        }
    }
}

fn ints(v: &Vector) -> Option<Side<'_, i64>> {
    match v {
        Vector::Int(d, n) => Some(Side::Vec(d, n.as_deref())),
        Vector::Const(Value::Int(i)) => Some(Side::Const(*i)),
        _ => None,
    }
}

/// `v` as doubles: an integer vector through `widened`, its widening.
fn as_floats<'a>(v: &'a Vector, widened: &'a Option<Vec<f64>>) -> Option<Side<'a, f64>> {
    match v {
        Vector::Float(d, n) => Some(Side::Vec(d, n.as_deref())),
        Vector::Const(Value::Float(f)) => Some(Side::Const(*f)),
        Vector::Const(Value::Int(i)) => Some(Side::Const(*i as f64)),
        Vector::Int(_, n) => widened.as_deref().map(|w| Side::Vec(w, n.as_deref())),
        _ => None,
    }
}

fn widened(v: &Vector) -> Option<Vec<f64>> {
    match v {
        Vector::Int(d, _) => Some(d.iter().map(|&i| i as f64).collect()),
        _ => None,
    }
}

fn is_cmp(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq)
}

fn cmp_holds(op: BinaryOp, o: std::cmp::Ordering) -> bool {
    use BinaryOp::*;
    use std::cmp::Ordering::*;
    match op {
        Eq => o == Equal,
        NotEq => o != Equal,
        Lt => o == Less,
        LtEq => o != Greater,
        Gt => o == Greater,
        _ => o != Less,
    }
}

/// `x op y` over the rows `sel`.
fn binary(op: BinaryOp, x: &Vector, y: &Vector, len: usize, sel: &Sel) -> Result<Vector> {
    use BinaryOp::*;
    let arith = matches!(op, Add | Sub | Mul | Div | Mod);
    if arith || is_cmp(op) {
        if let (Some(a), Some(b)) = (ints(x), ints(y)) {
            return int_kernel(op, a, b, len, sel);
        }
        // Doubles, an integer side widened (as a row widens it).
        let float_like =
            |v: &Vector| matches!(v, Vector::Float(..) | Vector::Const(Value::Float(_)));
        if float_like(x) || float_like(y) {
            let (wx, wy) = (widened(x), widened(y));
            if let (Some(a), Some(b)) = (as_floats(x, &wx), as_floats(y, &wy)) {
                return float_kernel(op, a, b, len, sel);
            }
        }
    }
    let out = each(len, sel, |i| {
        crate::expr::binary(op, x.value(i), y.value(i))
    })?;
    Ok(Rc::try_unwrap(out).unwrap_or_else(|v| (*v).clone()))
}

fn int_kernel(op: BinaryOp, a: Side<i64>, b: Side<i64>, len: usize, sel: &Sel) -> Result<Vector> {
    use BinaryOp::*;
    let mut nulls = vec![false; len];
    let overflow = || Error::out_of_range("bigint out of range");
    let zero = || Error::division_by_zero("division by zero");
    if is_cmp(op) {
        let mut out = vec![false; len];
        for i in sel.iter() {
            match (a.at(i), b.at(i)) {
                (Some(x), Some(y)) => out[i] = cmp_holds(op, x.cmp(&y)),
                _ => nulls[i] = true,
            }
        }
        return Ok(Vector::Bool(out, nulls.contains(&true).then_some(nulls)));
    }
    let mut out = vec![0i64; len];
    for i in sel.iter() {
        let (Some(x), Some(y)) = (a.at(i), b.at(i)) else {
            nulls[i] = true;
            continue;
        };
        out[i] = match op {
            Add => x.checked_add(y).ok_or_else(overflow)?,
            Sub => x.checked_sub(y).ok_or_else(overflow)?,
            Mul => x.checked_mul(y).ok_or_else(overflow)?,
            Div if y == 0 => return Err(zero()),
            Div => x.checked_div(y).ok_or_else(overflow)?,
            _ if y == 0 => return Err(zero()),
            _ => x.checked_rem(y).unwrap_or(0),
        };
    }
    Ok(Vector::Int(out, nulls.contains(&true).then_some(nulls)))
}

fn float_kernel(op: BinaryOp, a: Side<f64>, b: Side<f64>, len: usize, sel: &Sel) -> Result<Vector> {
    use BinaryOp::*;
    let mut nulls = vec![false; len];
    if is_cmp(op) {
        let mut out = vec![false; len];
        for i in sel.iter() {
            match (a.at(i), b.at(i)) {
                (Some(x), Some(y)) => out[i] = cmp_holds(op, total_cmp(x, y)),
                _ => nulls[i] = true,
            }
        }
        return Ok(Vector::Bool(out, nulls.contains(&true).then_some(nulls)));
    }
    let mut out = vec![0f64; len];
    for i in sel.iter() {
        let (Some(x), Some(y)) = (a.at(i), b.at(i)) else {
            nulls[i] = true;
            continue;
        };
        if matches!(op, Div | Mod) && y == 0.0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        let r = match op {
            Add => x + y,
            Sub => x - y,
            Mul => x * y,
            Div => x / y,
            _ => x % y,
        };
        crate::expr::check_float(
            op,
            r.is_infinite(),
            r == 0.0,
            [x.is_infinite(), y.is_infinite()],
            [x == 0.0, y == 0.0],
        )?;
        out[i] = r;
    }
    Ok(Vector::Float(out, nulls.contains(&true).then_some(nulls)))
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
    let t = truths(&*eval(p, &b, &b.sel, cx)?, b.len, &b.sel)?;
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
    Values,
    Enum,
}

/// A vector being built.
enum Builder {
    Skip,
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Values(Vec<Value>),
}

impl Builder {
    fn new(kind: Kind, cap: usize) -> Self {
        match kind {
            Kind::Skip => Builder::Skip,
            Kind::Int => Builder::Int(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Kind::Float => Builder::Float(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Kind::Bool => Builder::Bool(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Kind::Values | Kind::Enum => Builder::Values(Vec::with_capacity(cap)),
        }
    }

    fn push_null(&mut self) {
        match self {
            Builder::Skip => {}
            Builder::Int(d, n) => {
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
            Builder::Values(v) => v.push(Value::Null),
        }
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
            (Builder::Values(vals), v) => vals.push(v),
            (_, v) => {
                // Not the column's usual kind: values from here on.
                let mut vals = std::mem::replace(self, Builder::Skip).into_values();
                vals.push(v);
                *self = Builder::Values(vals);
            }
        }
    }

    fn into_values(self) -> Vec<Value> {
        match self.finish() {
            Vector::Values(v) => v,
            v => {
                let n = match &v {
                    Vector::Int(d, _) => d.len(),
                    Vector::Float(d, _) => d.len(),
                    Vector::Bool(d, _) => d.len(),
                    _ => 0,
                };
                (0..n).map(|i| v.value(i)).collect()
            }
        }
    }

    fn finish(self) -> Vector {
        let nulls = |n: Vec<bool>| n.contains(&true).then_some(n);
        match self {
            Builder::Skip => Vector::Const(Value::Null),
            Builder::Int(d, n) => Vector::Int(d, nulls(n)),
            Builder::Float(d, n) => Vector::Float(d, nulls(n)),
            Builder::Bool(d, n) => Vector::Bool(d, nulls(n)),
            Builder::Values(v) => Vector::Values(v),
        }
    }
}

/// A table's rows, a batch at a time, decoded into vectors.
struct ScanSource<'a> {
    table: &'a Table,
    /// `None`: an empty key range.
    entries: Option<Entries<'a>>,
    kinds: Vec<Kind>,
    labels: Labels,
    ctx: &'a Ctx<'a>,
}

impl<'a> ScanSource<'a> {
    fn new(
        table: &'a Table,
        access: &'a Access,
        wanted: Option<&'a [bool]>,
        cx: Cx<'a>,
    ) -> Result<Self> {
        let entries = match crate::exec::key_range(table, access, &cx.env(&[]))? {
            None => None,
            Some((start, end, _)) => {
                let end = end
                    .as_deref()
                    .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
                Some(Entries::new(
                    cx.ctx,
                    std::ops::Bound::Included(&start),
                    end,
                )?)
            }
        };
        let kinds = table
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
                    DataType::Enum(_) => Kind::Enum,
                    _ => Kind::Values,
                }
            })
            .collect();
        Ok(Self {
            table,
            entries,
            kinds,
            labels: Labels::default(),
            ctx: cx.ctx,
        })
    }

    /// The next (at most) `max` rows; `None` at the end.
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>> {
        let Some(entries) = &mut self.entries else {
            return Ok(None);
        };
        let mut cols: Vec<Builder> = self.kinds.iter().map(|&k| Builder::new(k, max)).collect();
        let mut n = 0;
        while n < max {
            let Some((key, bytes)) = entries.next()? else {
                break;
            };
            crate::budget::charge_scan()?;
            decode_into(self.ctx, self.table, key, bytes, &mut cols)?;
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
                    self.labels
                        .fill(self.ctx.txn, self.table.columns[c].ty, x)?;
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

/// Appends a stored row's columns to `cols` (a value stored out of line
/// fetched only for a column read).
fn decode_into(ctx: &Ctx, t: &Table, key: &[u8], b: &[u8], cols: &mut [Builder]) -> Result<()> {
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
                col.push(crate::large::fetch(ctx.txn, t, &pk, c, len)?);
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
    source: ScanSource<'a>,
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
        let source = ScanSource::new(table, access, wanted.map(Vec::as_slice), cx)?;
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
            let Some(input) = self.source.next_batch(self.batch)? else {
                return Ok(None);
            };
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
    let Some(pipe) = Pipeline::new(plan, plan, ctx, outer, true)? else {
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
    Int(HashMap<i64, u32>, Option<u32>),
    /// Any keys.
    Any(HashMap<Vec<Value>, u32>),
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
                _ => Keys::Any(HashMap::new()),
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
        // A single integer key: hashed as an integer.
        let int_key = match keys {
            [k] if matches!(**k, Vector::Int(..)) => {
                if self.seen.is_empty() && matches!(self.keys, Keys::Any(_)) {
                    self.keys = Keys::Int(HashMap::new(), None);
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
    let Some(mut pipe) = Pipeline::new(input, top, ctx, outer, false)? else {
        return Ok(None);
    };
    let cx = Cx { ctx, outer };
    let mut g = Grouping::new(group, aggs);
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
    if g.seen.is_empty() && group.is_empty() {
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
