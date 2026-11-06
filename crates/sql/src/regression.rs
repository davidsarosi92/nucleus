//! Two variables (PLAN.md §M14.27 S3): `corr`, `covar_pop`, `covar_samp`
//! and the `regr_*` family — PostgreSQL's (the SQL standard's), its
//! argument order `(y, x)`, its types (doubles), its `NULL` and `NaN`
//! rules.
//!
//! Each is computed from exact sums — n, Σx, Σy, Σx², Σy², Σxy in
//! [`ExactSum`]s, a product added exactly (its significands' product) — so the
//! centred sums are exact integers:
//!
//! - Cxx = n·Σx² − (Σx)²,  Cyy = n·Σy² − (Σy)²,  Cxy = n·Σxy − Σx·Σy
//!
//! (PostgreSQL's Sxx = Cxx / n, …), and every result is one exact ratio
//! of them rounded once — `corr` the square root of the exact
//! Cxy² / (Cxx·Cyy), rounded once too.  PostgreSQL's Youngs–Cramer update
//! rounds a row at a time instead, its answer depending on the order.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::bigint::BigInt;
use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, GroupAccumulator, Registry, Returns, Signature,
};
use crate::fsum::ExactSum;
use crate::moments::{combine, ratio, sqrt_ratio};
use crate::types::{DataType, Value};
use crate::vector::Vector;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Corr,
    CovarPop,
    CovarSamp,
    Slope,
    Intercept,
    R2,
    Count,
    AvgX,
    AvgY,
    Sxx,
    Syy,
    Sxy,
}

const AGGREGATES: &[(Kind, &str)] = &[
    (Kind::Corr, "corr"),
    (Kind::CovarPop, "covar_pop"),
    (Kind::CovarSamp, "covar_samp"),
    (Kind::Slope, "regr_slope"),
    (Kind::Intercept, "regr_intercept"),
    (Kind::R2, "regr_r2"),
    (Kind::Count, "regr_count"),
    (Kind::AvgX, "regr_avgx"),
    (Kind::AvgY, "regr_avgy"),
    (Kind::Sxx, "regr_sxx"),
    (Kind::Syy, "regr_syy"),
    (Kind::Sxy, "regr_sxy"),
];

// The sums, by bit.
const X: u8 = 1;
const Y: u8 = 2;
const XX: u8 = 4;
const YY: u8 = 8;
const XY: u8 = 16;

impl Kind {
    /// The sums its result needs (the others are not kept).
    fn needs(self) -> u8 {
        match self {
            Kind::Count => 0,
            Kind::AvgX => X,
            Kind::AvgY => Y,
            Kind::Sxx => X | XX,
            Kind::Syy => Y | YY,
            Kind::Sxy | Kind::CovarPop | Kind::CovarSamp => X | Y | XY,
            Kind::Slope | Kind::Intercept => X | Y | XX | XY,
            Kind::Corr | Kind::R2 => X | Y | XX | YY | XY,
        }
    }
}

/// Registers the two-variable aggregates.
pub fn register(r: &mut Registry) -> Result<()> {
    for (k, name) in AGGREGATES {
        r.add_aggregate(&[name], Arc::new(TwoVar(*k, name)))?;
    }
    Ok(())
}

struct TwoVar(Kind, &'static str);

impl Aggregate for TwoVar {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }

    fn order_free(&self) -> bool {
        true
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if let Some(t) = args.iter().filter_map(|a| a.ty).find(|t| !t.is_number()) {
            return Err(Error::undefined_function(format!(
                "function {}({}) does not exist",
                self.name(),
                t
            )));
        }
        let returns = if self.0 == Kind::Count {
            DataType::BigInt
        } else {
            DataType::Float
        };
        Ok(Signature::new(
            // Numbers as they are, made doubles here (a batch's integers
            // without a `Value` each).
            vec![Arg::Untyped(DataType::Float), Arg::Untyped(DataType::Float)],
            Returns::Type(Some(returns)),
        ))
    }

    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One(self.0, State::new(self.0)))
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups {
            kind: self.0,
            states: Vec::new(),
        }))
    }
}

/// A group's sums.
#[derive(Clone, Debug, Default)]
struct State {
    n: u64,
    /// Which sums are kept ([`Kind::needs`]).
    needs: u8,
    /// Σx, Σy, Σx², Σy², Σxy (those needed).
    sums: [ExactSum; 5],
    /// An infinite or `NaN` x, or y, was added.
    nonfinite_x: bool,
    nonfinite_y: bool,
    /// A product of finite values past the doubles' range.
    overflow: bool,
}

/// `v` as a double, `None` for `NULL`.
fn double(v: &Value) -> Result<Option<f64>> {
    Ok(match v {
        Value::Null => None,
        Value::Float(x) => Some(*x),
        Value::Real(x) => Some(f64::from(*x)),
        Value::Int(i) => Some(*i as f64),
        Value::Numeric(d) => Some(d.to_f64()),
        v => {
            return Err(Error::internal(format!(
                "a regression over {}",
                crate::types::type_name(v)
            )));
        }
    })
}

impl State {
    fn new(kind: Kind) -> Self {
        State {
            needs: kind.needs(),
            ..State::default()
        }
    }

    /// Adds the row `(y, x)`.
    #[inline]
    fn add(&mut self, y: f64, x: f64) {
        self.n += 1;
        let need = self.needs;
        self.nonfinite_x |= !x.is_finite();
        self.nonfinite_y |= !y.is_finite();
        let [sx, sy, sxx, syy, sxy] = &mut self.sums;
        if need & X != 0 {
            sx.add(x);
        }
        if need & Y != 0 {
            sy.add(y);
        }
        let mut product = |s: &mut ExactSum, a: f64, b: f64| {
            if a.is_finite() && b.is_finite() {
                if (a * b).is_infinite() {
                    self.overflow = true;
                } else {
                    s.add_product(a, b);
                }
            }
        };
        if need & XX != 0 {
            product(sxx, x, x);
        }
        if need & YY != 0 {
            product(syy, y, y);
        }
        if need & XY != 0 {
            product(sxy, x, y);
        }
    }

    fn add_values(&mut self, y: &Value, x: &Value) -> Result<()> {
        if let (Some(y), Some(x)) = (double(y)?, double(x)?) {
            self.add(y, x);
        }
        Ok(())
    }

    fn merge(&mut self, o: &State) {
        self.n += o.n;
        self.nonfinite_x |= o.nonfinite_x;
        self.nonfinite_y |= o.nonfinite_y;
        self.overflow |= o.overflow;
        for (a, b) in self.sums.iter_mut().zip(&o.sums) {
            a.merge(b);
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 24 * 5);
        out.extend_from_slice(&self.n.to_le_bytes());
        out.push(
            u8::from(self.nonfinite_x)
                | (u8::from(self.nonfinite_y) << 1)
                | (u8::from(self.overflow) << 2),
        );
        for (i, s) in self.sums.iter().enumerate() {
            if self.needs & (1 << i) != 0 {
                let b = s.to_bytes();
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(&b);
            }
        }
        out
    }

    /// Adds a state [`State::to_bytes`] made; `None` if malformed.
    fn merge_bytes(&mut self, b: &[u8]) -> Option<()> {
        let (n, rest) = b.split_first_chunk::<8>()?;
        let (&flags, mut rest) = rest.split_first()?;
        let needs = self.needs;
        for (i, s) in self.sums.iter_mut().enumerate() {
            if needs & (1 << i) != 0 {
                let (len, r) = rest.split_first_chunk::<4>()?;
                let (bytes, r) = r.split_at_checked(u32::from_le_bytes(*len) as usize)?;
                s.merge_bytes(bytes)?;
                rest = r;
            }
        }
        if !rest.is_empty() {
            return None;
        }
        self.n += u64::from_le_bytes(*n);
        self.nonfinite_x |= flags & 1 != 0;
        self.nonfinite_y |= flags & 2 != 0;
        self.overflow |= flags & 4 != 0;
        Some(())
    }

    /// An exact sum as `v · 2^e`.
    fn exact(&self, i: usize) -> Result<(BigInt, i64)> {
        self.sums[i]
            .exact()
            .ok_or_else(|| Error::internal("a nonfinite sum"))
    }

    /// `n·Σab − Σa·Σb`, exact.
    fn centred(&self, a: usize, b: usize, ab: usize) -> Result<(BigInt, i64)> {
        let n = BigInt::from_u128(u128::from(self.n));
        let (sa, ea) = self.exact(a)?;
        let (sb, eb) = self.exact(b)?;
        let (sab, eab) = self.exact(ab)?;
        Ok(combine(&[
            (n.mul(&sab), eab),
            (sa.mul(&sb).negated(), ea + eb),
        ]))
    }

    /// The result, by PostgreSQL's rules: `NULL` without rows (or with
    /// one, for `covar_samp`), and where x (or y) does not vary for the
    /// slope, intercept, r² and correlation; `NaN` from an infinite or
    /// `NaN` input; r² 1 for a horizontal line.
    fn finish(&self, kind: Kind) -> Result<Value> {
        let n = self.n;
        if kind == Kind::Count {
            return Ok(Value::Int(n as i64));
        }
        if n == 0 || (kind == Kind::CovarSamp && n == 1) {
            return Ok(Value::Null);
        }
        if self.overflow {
            return Err(crate::types::float_overflow());
        }
        let (nfx, nfy) = (self.nonfinite_x, self.nonfinite_y);
        let nb = BigInt::from_u128(u128::from(n));
        let int = |k: u128| (BigInt::from_u128(k), 0i64);
        let mul = |a: &(BigInt, i64), b: &(BigInt, i64)| (a.0.mul(&b.0), a.1 + b.1);
        let r = match kind {
            Kind::Count => unreachable!(),
            Kind::AvgX | Kind::AvgY => {
                let (i, nf) = if kind == Kind::AvgX {
                    (0, nfx)
                } else {
                    (1, nfy)
                };
                if nf {
                    // An infinite or NaN sum, divided.
                    let t = self.sums[i].total().unwrap_or(f64::NAN);
                    return Ok(Value::Float(t / n as f64));
                }
                ratio(&self.exact(i)?, &int(u128::from(n)))
            }
            Kind::Sxx | Kind::Syy => {
                let x = kind == Kind::Sxx;
                if (x && nfx) || (!x && nfy) {
                    return Ok(Value::Float(f64::NAN));
                }
                let c = if x {
                    self.centred(0, 0, 2)?
                } else {
                    self.centred(1, 1, 3)?
                };
                ratio(&c, &int(u128::from(n)))
            }
            Kind::Sxy | Kind::CovarPop | Kind::CovarSamp => {
                if nfx || nfy {
                    return Ok(Value::Float(f64::NAN));
                }
                let d = match kind {
                    Kind::Sxy => u128::from(n),
                    Kind::CovarPop => u128::from(n) * u128::from(n),
                    _ => u128::from(n) * u128::from(n - 1),
                };
                ratio(&self.centred(0, 1, 4)?, &int(d))
            }
            Kind::Slope | Kind::Intercept | Kind::R2 | Kind::Corr => {
                // Whether x, or y, does not vary (not when it is NaN).
                let cxx = if nfx {
                    None
                } else {
                    Some(self.centred(0, 0, 2)?)
                };
                let flat_x = cxx.as_ref().is_some_and(|c| c.0.is_zero());
                let cyy = if nfy || !matches!(kind, Kind::R2 | Kind::Corr) {
                    None
                } else {
                    Some(self.centred(1, 1, 3)?)
                };
                let flat_y = cyy.as_ref().is_some_and(|c| c.0.is_zero());
                if flat_x || (kind == Kind::Corr && flat_y) {
                    return Ok(Value::Null);
                }
                if kind == Kind::R2 && flat_y {
                    return Ok(Value::Float(1.0));
                }
                if nfx || nfy {
                    return Ok(Value::Float(f64::NAN));
                }
                let cxx = cxx.expect("a finite x");
                let cxy = self.centred(0, 1, 4)?;
                match kind {
                    Kind::Slope => ratio(&cxy, &cxx),
                    Kind::Intercept => {
                        // (Σy·Cxx − Σx·Cxy) / (n·Cxx).
                        let sy = self.exact(1)?;
                        let sx = self.exact(0)?;
                        let a = mul(&sy, &cxx);
                        let b = mul(&sx, &cxy);
                        let top = combine(&[a, (b.0.negated(), b.1)]);
                        ratio(&top, &(nb.mul(&cxx.0), cxx.1))
                    }
                    Kind::R2 => {
                        let cyy = cyy.expect("a finite y");
                        ratio(&mul(&cxy, &cxy), &mul(&cxx, &cyy))
                    }
                    _ => {
                        let cyy = cyy.expect("a finite y");
                        let r = sqrt_ratio(&mul(&cxy, &cxy), &mul(&cxx, &cyy));
                        if cxy.0.is_negative() { -r } else { r }
                    }
                }
            }
        };
        if r.is_finite() {
            Ok(Value::Float(r))
        } else {
            Err(crate::types::float_overflow())
        }
    }
}

/// A batch's doubles and which are `NULL`, from a vector of doubles or
/// integers (`None`: another kind, read a value at a time).
fn doubles(v: &Vector) -> Option<(std::borrow::Cow<'_, [f64]>, Option<&[bool]>)> {
    match v {
        Vector::Float(x, n) => Some((x.as_slice().into(), n.as_deref())),
        Vector::Int(x, n) => Some((
            x.iter().map(|&i| i as f64).collect::<Vec<_>>().into(),
            n.as_deref(),
        )),
        _ => None,
    }
}

/// One group's accumulator (the row-at-a-time path).
struct One(Kind, State);

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.1.add_values(&args[0], &args[1])
    }

    fn finish(&mut self) -> Result<Value> {
        self.1.finish(self.0)
    }
}

/// Many groups' states (the vectorized path).
struct Groups {
    kind: Kind,
    states: Vec<State>,
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        if groups > self.states.len() {
            self.states.resize(groups, State::new(self.kind));
        }
    }

    fn group_bytes(&self) -> usize {
        std::mem::size_of::<State>() + 32 * self.kind.needs().count_ones() as usize
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let st = &mut self.states;
        match (doubles(args[0]), doubles(args[1])) {
            (Some((y, yn)), Some((x, xn))) => {
                for (&r, &g) in rows.iter().zip(groups) {
                    let r = r as usize;
                    if !yn.is_some_and(|n| n[r]) && !xn.is_some_and(|n| n[r]) {
                        st[g as usize].add(y[r], x[r]);
                    }
                }
            }
            _ => {
                let (ys, xs) = (args[0], args[1]);
                for (&r, &g) in rows.iter().zip(groups) {
                    let r = r as usize;
                    st[g as usize].add_values(&ys.value(r), &xs.value(r))?;
                }
            }
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(Value::Bytea(self.states[group].to_bytes().into()))
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let bad = || Error::internal("a malformed partial regression");
        let Value::Bytea(b) = partial else {
            return Err(bad());
        };
        let mut o = State::new(self.kind);
        o.merge_bytes(&b).ok_or_else(bad)?;
        self.states[group].merge(&o);
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.states[group].finish(self.kind)
    }

    fn finish_keeps_state(&self) -> bool {
        true
    }
}
