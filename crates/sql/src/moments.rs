//! Moments (PLAN.md §M14.27 S2): `var_samp` / `variance`, `var_pop`,
//! `stddev_samp` / `stddev`, `stddev_pop` (PostgreSQL's), `skewness` and
//! `kurtosis` (DuckDB's, sample-corrected).
//!
//! Each is computed from exact power sums, so its result is the same bits
//! however the rows are split among workers and batches:
//!
//! - Doubles: Σx, Σx², Σx³, Σx⁴ in [`ExactSum`]s — a power as its rounded
//!   value and the rounding's error, from fused multiply-adds, so the sums
//!   are exact (unless an error falls below 2⁻¹⁰⁷⁴).  A variance is the
//!   exact `(n·Σx² − (Σx)²) / (n·(n − 1))` rounded once (PostgreSQL's
//!   Youngs–Cramer update, DuckDB's Welford and the textbook formula each
//!   lose digits, measured in the study); a standard deviation its square
//!   root.  Skewness and kurtosis take the exact central sums, then
//!   DuckDB's formula in doubles.
//! - Integers and decimals (PostgreSQL's `numeric` results): Σx and Σx²
//!   in wide integers, then PostgreSQL's own steps — the quotient at
//!   `select_div_scale`'s scale, rounded half away from zero, and the
//!   standard deviation the square root of that, rounded at the same
//!   scale.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::bigint::{BigInt, Wide, square_u256};
use crate::decimal::{Decimal, LIMIT, MAX_DIGITS};
use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, GroupAccumulator, Registry, Returns, Signature,
};
use crate::fsum::ExactSum;
use crate::types::{DataType, Value};
use crate::vector::Vector;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
    Skewness,
    Kurtosis,
}

const AGGREGATES: &[(Kind, &[&str])] = &[
    (Kind::VarSamp, &["var_samp", "variance"]),
    (Kind::VarPop, &["var_pop"]),
    (Kind::StddevSamp, &["stddev_samp", "stddev"]),
    (Kind::StddevPop, &["stddev_pop"]),
    (Kind::Skewness, &["skewness", "skew"]),
    (Kind::Kurtosis, &["kurtosis"]),
];

impl Kind {
    /// The highest power summed.
    fn order(self) -> usize {
        match self {
            Kind::Skewness => 3,
            Kind::Kurtosis => 4,
            _ => 2,
        }
    }

    /// Whether integers and decimals keep `numeric` (PostgreSQL's
    /// variances) rather than becoming doubles (DuckDB's moments).
    fn numeric(self) -> bool {
        self.order() == 2
    }
}

/// Registers the moments.
pub fn register(r: &mut Registry) -> Result<()> {
    // Each name its own registration, so an error names the one called.
    for (k, names) in AGGREGATES {
        for name in *names {
            r.add_aggregate(&[name], Arc::new(Moment(*k, name)))?;
        }
    }
    Ok(())
}

struct Moment(Kind, &'static str);

impl Aggregate for Moment {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }

    fn order_free(&self) -> bool {
        true
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::*;
        let at = args[0].ty;
        if at.is_some_and(|t| !t.is_number()) {
            return Err(Error::undefined_function(format!(
                "function {}({}) does not exist",
                self.name(),
                at.unwrap()
            )));
        }
        let exact = self.0.numeric() && at.is_none_or(|t| t.is_integer() || t == Numeric);
        Ok(if exact {
            Signature::new(vec![Arg::Untyped(Numeric)], Returns::Type(Some(Numeric)))
        } else {
            Signature::new(vec![Arg::Convert(Float)], Returns::Type(Some(Float)))
        })
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
#[derive(Clone, Debug)]
struct State {
    n: u64,
    /// Its rows were doubles (else integers or decimals).
    float: bool,
    /// Doubles: Σxᵏ for k = 1 ..= the order.
    f: Vec<ExactSum>,
    /// An infinite or `NaN` value was added (the result is `NaN`).
    nonfinite: bool,
    /// A finite value's power is past the doubles' range, or a decimal
    /// sum past the wide integers'.
    overflow: bool,
    /// Integers and decimals: Σx and Σx² as integers at `scale` and
    /// `2·scale`.
    scale: u32,
    s1: Wide<8>,
    s2: Wide<8>,
}

impl State {
    fn new(kind: Kind) -> Self {
        State {
            n: 0,
            float: false,
            f: vec![ExactSum::new(); kind.order()],
            nonfinite: false,
            overflow: false,
            scale: 0,
            s1: Wide::default(),
            s2: Wide::default(),
        }
    }

    #[inline]
    fn add_f(&mut self, x: f64) {
        self.n += 1;
        self.float = true;
        if !x.is_finite() {
            self.nonfinite = true;
            return;
        }
        let order = self.f.len();
        self.f[0].add(x);
        let h = x * x;
        if !h.is_finite() {
            self.overflow = true;
            return;
        }
        let l = x.mul_add(x, -h);
        self.f[1].add(h);
        self.f[1].add(l);
        if order >= 3 {
            // x³ = h·x + l·x.
            if !(h * x).is_finite() {
                self.overflow = true;
                return;
            }
            self.f[2].add_product(h, x);
            self.f[2].add_product(l, x);
        }
        if order >= 4 {
            // x⁴ = h² + 2·h·l + l².
            if !(h * h).is_finite() {
                self.overflow = true;
                return;
            }
            self.f[3].add_product(h, h);
            self.f[3].add_product(2.0 * h, l);
            self.f[3].add_product(l, l);
        }
    }

    /// Adds a decimal `m · 10^−s` (an integer: `s` 0).
    #[inline]
    fn add_dec(&mut self, m: i128, s: u32) {
        self.n += 1;
        if s > self.scale {
            self.rescale(s);
        }
        let k = self.scale - s;
        match 10i128.checked_pow(k).and_then(|p| m.checked_mul(p)) {
            Some(m) => {
                self.s1.add_i128(m);
                let (hi, lo) = square_u256(m);
                self.s2.add_u256(hi, lo);
            }
            None => {
                let m = BigInt::from_i128(m).mul_pow10(k);
                match (Wide::from_big(&m), Wide::from_big(&m.mul(&m))) {
                    (Some(a), Some(b)) => {
                        self.s1.add(&a);
                        self.s2.add(&b);
                    }
                    _ => self.overflow = true,
                }
            }
        }
    }

    /// Brings the decimal sums to scale `s` (larger).
    fn rescale(&mut self, s: u32) {
        let k = s - self.scale;
        match (self.s1.mul_pow10(k), self.s2.mul_pow10(2 * k)) {
            (Some(a), Some(b)) => {
                self.s1 = a;
                self.s2 = b;
            }
            _ => self.overflow = true,
        }
        self.scale = s;
    }

    fn add_value(&mut self, v: &Value) -> Result<()> {
        match v {
            Value::Null => {}
            Value::Float(x) => self.add_f(*x),
            Value::Real(x) => self.add_f(f64::from(*x)),
            Value::Int(i) if self.f.len() == 2 => self.add_dec(i128::from(*i), 0),
            Value::Numeric(d) if self.f.len() == 2 => self.add_dec(d.mantissa(), d.scale()),
            Value::Int(i) => self.add_f(*i as f64),
            Value::Numeric(d) => self.add_f(d.to_f64()),
            v => {
                return Err(Error::internal(format!(
                    "a moment of {}",
                    crate::types::type_name(v)
                )));
            }
        }
        Ok(())
    }

    fn merge(&mut self, o: &State) {
        self.n += o.n;
        self.float |= o.float;
        self.nonfinite |= o.nonfinite;
        self.overflow |= o.overflow;
        for (a, b) in self.f.iter_mut().zip(&o.f) {
            a.merge(b);
        }
        if o.scale > self.scale {
            self.rescale(o.scale);
        } else if o.scale < self.scale {
            let mut o = o.clone();
            o.rescale(self.scale);
            self.overflow |= o.overflow;
            self.s1.add(&o.s1);
            self.s2.add(&o.s2);
            return;
        }
        self.s1.add(&o.s1);
        self.s2.add(&o.s2);
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.n.to_le_bytes());
        // The decimal sums only when there are any (not for doubles).
        let wide = self.s1.0.iter().chain(&self.s2.0).any(|&w| w != 0);
        out.push(
            u8::from(self.nonfinite)
                | (u8::from(self.overflow) << 1)
                | (u8::from(self.float) << 2)
                | (u8::from(wide) << 3),
        );
        out.extend_from_slice(&self.scale.to_le_bytes());
        if wide {
            for w in self.s1.0.iter().chain(&self.s2.0) {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        for f in &self.f {
            let b = f.to_bytes();
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(&b);
        }
        out
    }

    fn from_bytes(kind: Kind, b: &[u8]) -> Option<State> {
        let mut s = State::new(kind);
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let r = b.get(*at..*at + n)?;
            *at += n;
            Some(r)
        };
        let mut at = 0;
        s.n = u64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?);
        let flags = take(&mut at, 1)?[0];
        s.nonfinite = flags & 1 != 0;
        s.overflow = flags & 2 != 0;
        s.float = flags & 4 != 0;
        s.scale = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
        for i in (0..16).filter(|_| flags & 8 != 0) {
            let w = u64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?);
            if i < 8 {
                s.s1.0[i] = w;
            } else {
                s.s2.0[i - 8] = w;
            }
        }
        for f in &mut s.f {
            let len = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
            *f = ExactSum::from_bytes(take(&mut at, len)?)?;
        }
        (at == b.len()).then_some(s)
    }

    /// The result: `numeric` when the rows were integers or decimals and
    /// the aggregate keeps them so.
    fn finish(&self, kind: Kind) -> Result<Value> {
        if kind.numeric() && !self.float {
            return self.finish_numeric(kind);
        }
        let n = self.n;
        let least = match kind {
            Kind::VarSamp | Kind::StddevSamp => 2,
            Kind::VarPop | Kind::StddevPop => 1,
            Kind::Skewness => 3,
            Kind::Kurtosis => 4,
        };
        if n < least {
            return Ok(Value::Null);
        }
        if self.nonfinite {
            return Ok(Value::Float(f64::NAN));
        }
        if self.overflow {
            return Err(crate::types::float_overflow());
        }
        let sums: Vec<(BigInt, i64)> = self
            .f
            .iter()
            .map(|s| s.exact().ok_or_else(|| Error::internal("a nonfinite sum")))
            .collect::<Result<_>>()?;
        let nb = BigInt::from_u128(u128::from(n));
        // C2 = n·Σx² − (Σx)², exact, as v · 2^e.
        let (s1, e1) = &sums[0];
        let (s2, e2) = &sums[1];
        let c2 = combine(&[(nb.mul(s2), *e2), (s1.mul(s1).negated(), 2 * e1)]);
        match kind {
            Kind::VarSamp | Kind::VarPop | Kind::StddevSamp | Kind::StddevPop => {
                let d = u128::from(n)
                    * u128::from(if matches!(kind, Kind::VarSamp | Kind::StddevSamp) {
                        n - 1
                    } else {
                        n
                    });
                let var = quotient(&c2, d);
                Ok(Value::Float(match kind {
                    Kind::StddevSamp | Kind::StddevPop => var.sqrt(),
                    _ => var,
                }))
            }
            Kind::Skewness => {
                // C3 = n²·Σx³ − 3n·Σx²·Σx + 2(Σx)³; skewness =
                // √(n(n−1))/(n−2) · C3 / C2^1.5 (DuckDB's, the n³ gone).
                let (s3, e3) = &sums[2];
                let c3 = combine(&[
                    (nb.mul(&nb).mul(s3), *e3),
                    (BigInt::from_i128(-3).mul(&nb).mul(s2).mul(s1), e2 + e1),
                    (BigInt::from_i128(2).mul(s1).mul(s1).mul(s1), 3 * e1),
                ]);
                // No spread: no skew to measure (DuckDB's NULL).
                if c2.0.is_zero() || c2.0.is_negative() {
                    return Ok(Value::Null);
                }
                let (m2, x2) = mantissa(&c2);
                let (m3, x3) = mantissa(&c3);
                if m3 == 0.0 {
                    return Ok(Value::Float(0.0));
                }
                // C3 / C2^1.5 = m3/m2^1.5 · 2^(x3 − 1.5·x2).
                let half = if x2 % 2 == 0 { 1.0 } else { 2f64.sqrt() };
                let ratio = m3 / (m2 * m2.sqrt() * half)
                    * pow2(x3 - 3 * x2.div_euclid(2) - (x2.rem_euclid(2)));
                let n = n as f64;
                let r = (n * (n - 1.0)).sqrt() / (n - 2.0) * ratio;
                finite(r, "skewness")
            }
            Kind::Kurtosis => {
                // C4 = n³Σx⁴ − 4n²Σx³Σx + 6nΣx²(Σx)² − 3(Σx)⁴ (n⁴·m4).
                let (s3, e3) = &sums[2];
                let (s4, e4) = &sums[3];
                if c2.0.is_zero() || c2.0.is_negative() {
                    return Ok(Value::Null);
                }
                let s1sq = s1.mul(s1);
                let c4 = combine(&[
                    (nb.mul(&nb).mul(&nb).mul(s4), *e4),
                    (
                        BigInt::from_i128(-4).mul(&nb).mul(&nb).mul(s3).mul(s1),
                        e3 + e1,
                    ),
                    (
                        BigInt::from_i128(6).mul(&nb).mul(s2).mul(&s1sq),
                        e2 + 2 * e1,
                    ),
                    (BigInt::from_i128(-3).mul(&s1sq).mul(&s1sq), 4 * e1),
                ]);
                // m4/m2² = C4/C2² (both over n⁴), so DuckDB's formula is
                // (n−1)·((n+1)·C4 − 3(n−1)·C2²) / ((n−2)(n−3)·C2²): its
                // numerator exact — no cancellation left to the doubles.
                let c2sq = (c2.0.mul(&c2.0), 2 * c2.1);
                let top = combine(&[
                    (BigInt::from_u128(u128::from(n) + 1).mul(&c4.0), c4.1),
                    (
                        BigInt::from_u128(3 * (u128::from(n) - 1))
                            .mul(&c2sq.0)
                            .negated(),
                        c2sq.1,
                    ),
                ]);
                let (mt, xt) = mantissa(&top);
                let (mb, xb) = mantissa(&c2sq);
                let n = n as f64;
                let r = (n - 1.0) * (mt / mb) * pow2(xt - xb) / ((n - 2.0) * (n - 3.0));
                finite(r, "kurtosis")
            }
        }
    }

    /// PostgreSQL's `numeric_stddev_internal`, on exact sums.
    fn finish_numeric(&self, kind: Kind) -> Result<Value> {
        let n = self.n;
        let sample = matches!(kind, Kind::VarSamp | Kind::StddevSamp);
        if n == 0 || (sample && n == 1) {
            return Ok(Value::Null);
        }
        if self.overflow || !self.s1.in_range() || !self.s2.in_range() {
            return Err(crate::decimal::overflow());
        }
        let s1 = self.s1.big();
        let s2 = self.s2.big();
        // N·Σx² − (Σx)², at scale 2s.
        let num = BigInt::from_u128(u128::from(n)).mul(&s2).sub(&s1.mul(&s1));
        if num.is_zero() || num.is_negative() {
            return Ok(Value::Numeric(Decimal::from_i64(0)));
        }
        let den = u128::from(n) * u128::from(if sample { n - 1 } else { n });
        let dscale = 2 * self.scale;
        let rscale = select_div_scale(&num, dscale, den)
            .clamp(i64::from(dscale), i64::from(MAX_DIGITS)) as u32;
        let rscale = rscale.min(MAX_DIGITS);
        // The variance's mantissa at rscale, rounded half away from zero.
        let q = num.mul_pow10(rscale.saturating_sub(dscale)).div_round(den);
        let q = if matches!(kind, Kind::StddevSamp | Kind::StddevPop) {
            // √(q · 10^−r) at scale r: √(q · 10^r), one digit more, rounded.
            let r10 = q.mul_pow10(rscale + 2).isqrt();
            r10.div_round(10)
        } else {
            q
        };
        let m = q
            .to_i128()
            .filter(|m| m.unsigned_abs() <= LIMIT)
            .ok_or_else(crate::decimal::overflow)?;
        Ok(Value::Numeric(Decimal::new(m, rscale)?))
    }
}

/// `Σ vᵢ · 2^eᵢ` as one `v · 2^e`.
fn combine(terms: &[(BigInt, i64)]) -> (BigInt, i64) {
    let e = terms.iter().map(|t| t.1).min().unwrap_or(0);
    let v = terms.iter().fold(BigInt::zero(), |acc, (v, x)| {
        acc.add(&v.shl((x - e) as u64))
    });
    (v, e)
}

/// `(v · 2^e) / d`, rounded once to a double.
fn quotient(c: &(BigInt, i64), d: u128) -> f64 {
    let (v, e) = c;
    if v.is_zero() || v.is_negative() {
        return 0.0;
    }
    // Enough quotient bits to round on: 55 and more.
    let dbits = 128 - u64::from(d.leading_zeros());
    let t = (dbits + 56).saturating_sub(v.bits());
    let (q, r) = v.shl(t).divmod_mag(d);
    q.to_f64(e - t as i64, r != 0)
}

/// `v · 2^e` as `m · 2^x`, `m` in [1, 2) (rounded once).
fn mantissa(c: &(BigInt, i64)) -> (f64, i64) {
    let (v, e) = c;
    if v.is_zero() {
        return (0.0, 0);
    }
    let b = v.bits() as i64 - 1;
    (v.to_f64(-b, false), b + e)
}

fn pow2(x: i64) -> f64 {
    // In steps that stay inside the doubles' range.
    let mut r = 1.0;
    let mut x = x;
    while x > 1000 {
        r *= 2f64.powi(1000);
        x -= 1000;
    }
    while x < -1000 {
        r *= 2f64.powi(-1000);
        x += 1000;
    }
    r * 2f64.powi(x as i32)
}

fn finite(r: f64, what: &str) -> Result<Value> {
    if r.is_finite() {
        Ok(Value::Float(r))
    } else {
        Err(Error::out_of_range(format!("{what} is out of range")))
    }
}

/// PostgreSQL's `select_div_scale` for `num` (at decimal scale `dscale`)
/// divided by the integer `den`: at least 16 significant digits, and no
/// less than either operand's scale.
fn select_div_scale(num: &BigInt, dscale: u32, den: u128) -> i64 {
    let (w1, d1) = nbase_lead(&num.to_decimal(), dscale);
    let (w2, d2) = nbase_lead(&den.to_string(), 0);
    let mut qweight = w1 - w2;
    if d1 <= d2 {
        qweight -= 1;
    }
    (16 - qweight * 4).max(i64::from(dscale)).clamp(0, 1000)
}

/// PostgreSQL's base-10000 weight and leading base-10000 digit of the
/// decimal `digits · 10^−scale` (not zero).
fn nbase_lead(digits: &str, scale: u32) -> (i64, u32) {
    let digits = digits.trim_start_matches('0');
    // The leading digit's power of ten.
    let e = digits.len() as i64 - 1 - i64::from(scale);
    let weight = e.div_euclid(4);
    // The digits from 10^e down to 10^(4·weight).
    let take = (e - 4 * weight + 1) as usize;
    let lead: String = digits
        .chars()
        .chain(std::iter::repeat('0'))
        .take(take)
        .collect();
    (weight, lead.parse().unwrap_or(0))
}

/// One group's accumulator (the row-at-a-time path).
struct One(Kind, State);

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.1.add_value(&args[0])
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
        std::mem::size_of::<State>() + 64 * self.kind.order()
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let st = &mut self.states;
        let pairs = rows
            .iter()
            .zip(groups)
            .map(|(&r, &g)| (r as usize, g as usize));
        let decimal = self.kind.numeric();
        match args[0] {
            Vector::Float(v, _) => pairs.for_each(|(r, g)| st[g].add_f(v[r])),
            Vector::Int(v, _) if decimal => {
                pairs.for_each(|(r, g)| st[g].add_dec(i128::from(v[r]), 0))
            }
            Vector::Int(v, _) => pairs.for_each(|(r, g)| st[g].add_f(v[r] as f64)),
            Vector::Dec(s, m, _) if decimal => pairs.for_each(|(r, g)| st[g].add_dec(m.at(r), *s)),
            Vector::Const(v) => {
                for (_, g) in pairs {
                    st[g].add_value(v)?;
                }
            }
            Vector::Values(vals) => {
                for (r, g) in pairs {
                    st[g].add_value(&vals[r])?;
                }
            }
            other => {
                for (r, g) in pairs {
                    st[g].add_value(&other.value(r))?;
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
        let Value::Bytea(b) = partial else {
            return Err(Error::internal("a malformed partial moment"));
        };
        let o = State::from_bytes(self.kind, &b)
            .ok_or_else(|| Error::internal("a malformed partial moment"))?;
        self.states[group].merge(&o);
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.states[group].finish(self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quotient rounded on its remainder: 1 + 2⁻⁵³ + 2⁻⁶³/3 is just
    /// above the tie between 1 and 1 + 2⁻⁵², so it rounds up.
    #[test]
    fn quotients_round_on_their_remainder() {
        let k = 10;
        let v = BigInt::from_u128(3 * ((1u128 << 53) + 1))
            .shl(k)
            .add(&BigInt::from_u128(1));
        assert_eq!(quotient(&(v, -(53 + k as i64)), 3), 1.0 + f64::EPSILON);
        let tie = BigInt::from_u128(3 * ((1u128 << 53) + 1)).shl(k);
        assert_eq!(quotient(&(tie, -(53 + k as i64)), 3), 1.0);
    }

    #[test]
    fn postgresql_division_scales() {
        // 6 / 6: the leading digits equal, one more group of digits.
        assert_eq!(select_div_scale(&BigInt::from_u128(6), 0, 6), 20);
        assert_eq!(select_div_scale(&BigInt::from_u128(20), 0, 12), 16);
        assert_eq!(nbase_lead("12345", 0), (1, 1));
        assert_eq!(nbase_lead("12345", 2), (0, 123));
        assert_eq!(nbase_lead("5", 6), (-2, 500));
    }
}
