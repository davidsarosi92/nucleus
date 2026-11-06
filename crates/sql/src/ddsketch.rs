//! Approximate quantiles within a relative error (PLAN.md §M14.27 S6):
//! DDSketch (Masson, Rim, Lee, VLDB 2019).
//!
//! A value `x > 0` falls in the bucket `⌊L(x) · k⌋` of logarithmic
//! buckets, each spanning at most a factor `γ = (1 + α) / (1 − α)`; a
//! bucket's value `lo · (1 + α)` is within `α · x` of every `x` in it, so
//! every quantile is within a relative error `α` (1 % by default) of the
//! value at its rank.  Negative values have buckets of their own, zero
//! (and what is too small to tell from it) one, the infinities and NaN
//! each a count — NaN the greatest, as SQL orders it.
//!
//! `L` is DDSketch's cubically interpolated logarithm (DataDog's
//! `CubicallyInterpolatedMapping`): the exponent, plus a cubic of the
//! significand — `+` and `*` only, so a value's bucket is the same on
//! every machine, where `ln` would be the platform's.  `k` comes from `ln
//! γ = 2 atanh(α)` summed as a series, a bucket's lower bound from the
//! cubic inverted by Newton's method in a fixed number of steps: no
//! library function anywhere.
//!
//! At most 2048 buckets a sign: past that the smallest magnitudes are
//! folded into the least kept (DDSketch's collapsing store) — which
//! bucket that is depends only on the greatest one, so a sketch is a
//! function of its values whatever their order.  Sketches merge by
//! adding counts.
//!
//! `approx_percentile(x, f)` (also `approx_quantile`, DuckDB's name), `f`
//! a fraction or an array of them; `ddsketch_agg(x [, α])` the sketch as
//! a value of the registered type `ddsketch`, `ddsketch_merge(s)`,
//! `ddsketch_quantile(s, f)`, `ddsketch_count(s)`.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, CanonicalBytes, Equality, GroupAccumulator,
    Hashing, Registry, Returns, ScalarFunction, Signature, Type, TypeRef, Volatility,
};
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// The default relative error.
pub const ALPHA: f64 = 0.01;
/// Buckets a sign keeps at most.
const MAX_BINS: i64 = 2048;
const VERSION: u8 = 1;

// The cubic: P(s) = ((A s + B) s + C) s, P(0) = 0, P(1) = 1.
const A: f64 = 6.0 / 35.0;
const B: f64 = -3.0 / 5.0;
const C: f64 = 10.0 / 7.0;

/// `ln((1 + α) / (1 − α)) = 2 atanh(α)`, by its series.
fn ln_gamma(alpha: f64) -> f64 {
    let a2 = alpha * alpha;
    let (mut sum, mut power, mut k) = (alpha, alpha, 1.0);
    loop {
        power *= a2;
        k += 2.0;
        let next = sum + power / k;
        if next == sum {
            return 2.0 * sum;
        }
        sum = next;
    }
}

/// The interpolated `log₂` of a positive normal double.
#[inline]
fn log2ish(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let s = (bits & ((1 << 52) - 1)) as f64 / (1u64 << 52) as f64;
    ((A * s + B) * s + C) * s + e as f64
}

/// Its inverse.
fn exp2ish(l: f64) -> f64 {
    let e = l.floor();
    let t = l - e;
    // P(s) = t, P increasing on [0, 1]: Newton from s = t.
    let mut s = t;
    for _ in 0..8 {
        let f = ((A * s + B) * s + C) * s - t;
        let d = (3.0 * A * s + 2.0 * B) * s + C;
        s = (s - f / d).clamp(0.0, 1.0);
    }
    let pow2 = |e: i64| f64::from_bits(((e + 1023) as u64) << 52);
    match e as i64 {
        e if e > 1023 => f64::INFINITY,
        e if e >= -1022 => (1.0 + s) * pow2(e),
        // Below the normal range: in two steps, the last one rounding.
        e if e >= -1022 - 52 => (1.0 + s) * pow2(e + 64) * pow2(-64),
        _ => 0.0,
    }
}

/// Buckets of one sign: counts from index `offset`.
#[derive(Clone, Debug, Default, PartialEq)]
struct Store {
    offset: i64,
    counts: Vec<u64>,
}

impl Store {
    fn top(&self) -> i64 {
        self.offset + self.counts.len() as i64 - 1
    }

    fn add(&mut self, i: i64, n: u64) {
        if self.counts.is_empty() {
            self.offset = i;
            self.counts.push(n);
            return;
        }
        let top = self.top().max(i);
        let floor = top - MAX_BINS + 1;
        // The kept range's floor rises: what is below it folds into it.
        if floor > self.offset {
            let cut = ((floor - self.offset) as usize).min(self.counts.len());
            let folded: u64 = self.counts.drain(..cut).sum();
            self.offset = floor;
            if self.counts.is_empty() {
                self.counts.push(0);
            }
            self.counts[0] += folded;
        }
        let i = i.max(floor);
        if i < self.offset {
            let more = (self.offset - i) as usize;
            self.counts.splice(0..0, std::iter::repeat_n(0, more));
            self.offset = i;
        } else if i > self.top() {
            let len = (i - self.offset + 1) as usize;
            self.counts.resize(len, 0);
        }
        self.counts[(i - self.offset) as usize] += n;
    }

    fn merge(&mut self, other: &Store) {
        for (k, &n) in other.counts.iter().enumerate() {
            if n > 0 {
                self.add(other.offset + k as i64, n);
            }
        }
    }

    fn total(&self) -> u64 {
        self.counts.iter().sum()
    }
}

/// A sketch.
#[derive(Clone, Debug, PartialEq)]
pub struct DdSketch {
    alpha: f64,
    multiplier: f64,
    pos: Store,
    neg: Store,
    zero: u64,
    neg_inf: u64,
    pos_inf: u64,
    nan: u64,
}

impl DdSketch {
    /// An empty sketch of relative accuracy `alpha` (in `(0, 1)`).
    pub fn new(alpha: f64) -> Result<DdSketch> {
        if !(alpha > 0.0 && alpha < 1.0) {
            return Err(Error::out_of_range(format!(
                "relative accuracy {alpha} is not between 0 and 1"
            )));
        }
        Ok(DdSketch {
            alpha,
            multiplier: 1.0 / (C * ln_gamma(alpha)),
            pos: Store::default(),
            neg: Store::default(),
            zero: 0,
            neg_inf: 0,
            pos_inf: 0,
            nan: 0,
        })
    }

    #[inline]
    fn index(&self, x: f64) -> i64 {
        (log2ish(x) * self.multiplier).floor() as i64
    }

    /// A bucket's value: its lower bound times `1 + α`.
    fn value(&self, i: i64) -> f64 {
        exp2ish(i as f64 / self.multiplier) * (1.0 + self.alpha)
    }

    /// Adds a value.
    #[inline]
    pub fn add(&mut self, x: f64) {
        if x.is_nan() {
            self.nan += 1;
        } else if x >= f64::MIN_POSITIVE {
            if x == f64::INFINITY {
                self.pos_inf += 1;
            } else {
                let i = self.index(x);
                self.pos.add(i, 1);
            }
        } else if x <= -f64::MIN_POSITIVE {
            if x == f64::NEG_INFINITY {
                self.neg_inf += 1;
            } else {
                let i = self.index(-x);
                self.neg.add(i, 1);
            }
        } else {
            self.zero += 1;
        }
    }

    /// The values it holds.
    pub fn count(&self) -> u64 {
        self.pos.total() + self.neg.total() + self.zero + self.neg_inf + self.pos_inf + self.nan
    }

    /// Adds `other`'s values (of the same accuracy).
    pub fn merge(&mut self, other: &DdSketch) -> Result<()> {
        if other.alpha.to_bits() != self.alpha.to_bits() {
            return Err(Error::invalid_parameter(format!(
                "cannot merge sketches of relative accuracy {} and {}",
                self.alpha, other.alpha
            )));
        }
        self.pos.merge(&other.pos);
        self.neg.merge(&other.neg);
        self.zero += other.zero;
        self.neg_inf += other.neg_inf;
        self.pos_inf += other.pos_inf;
        self.nan += other.nan;
        Ok(())
    }

    /// The value at fraction `f` (in `[0, 1]`): the bucket holding the
    /// value of rank `⌊f · (n − 1)⌋`; `None` when empty.
    pub fn quantile(&self, f: f64) -> Option<f64> {
        let n = self.count();
        if n == 0 {
            return None;
        }
        let rank = f * (n - 1) as f64;
        let mut seen = 0u64;
        let mut past = |c: u64| {
            seen += c;
            seen as f64 > rank
        };
        if past(self.neg_inf) {
            return Some(f64::NEG_INFINITY);
        }
        for (k, &c) in self.neg.counts.iter().enumerate().rev() {
            if c > 0 && past(c) {
                return Some(-self.value(self.neg.offset + k as i64));
            }
        }
        if past(self.zero) {
            return Some(0.0);
        }
        for (k, &c) in self.pos.counts.iter().enumerate() {
            if c > 0 && past(c) {
                return Some(self.value(self.pos.offset + k as i64));
            }
        }
        if past(self.pos_inf) {
            return Some(f64::INFINITY);
        }
        Some(f64::NAN)
    }

    /// The canonical bytes: version, α, the special counts, then each
    /// sign's buckets — offset and counts as LEB128 varints.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![VERSION];
        b.extend_from_slice(&self.alpha.to_le_bytes());
        for n in [self.zero, self.neg_inf, self.pos_inf, self.nan] {
            varint(&mut b, n);
        }
        for s in [&self.pos, &self.neg] {
            varint(&mut b, zigzag(s.offset));
            varint(&mut b, s.counts.len() as u64);
            for &c in &s.counts {
                varint(&mut b, c);
            }
        }
        b
    }

    /// A sketch from its canonical bytes.
    pub fn from_bytes(b: &[u8]) -> Result<DdSketch> {
        let bad = || Error::invalid_parameter("not a ddsketch");
        match b.first() {
            Some(&VERSION) => {}
            Some(v) => {
                return Err(Error::feature(format!(
                    "ddsketch format version {v} (this version reads {VERSION})"
                )));
            }
            None => return Err(bad()),
        }
        let alpha = f64::from_le_bytes(b.get(1..9).ok_or_else(bad)?.try_into().unwrap());
        let mut s = DdSketch::new(alpha).map_err(|_| bad())?;
        let mut r = &b[9..];
        let mut next = || unvarint(&mut r).ok_or_else(bad);
        s.zero = next()?;
        s.neg_inf = next()?;
        s.pos_inf = next()?;
        s.nan = next()?;
        for sign in 0..2 {
            let offset = unzigzag(next()?);
            let len = next()?;
            if len > MAX_BINS as u64 {
                return Err(bad());
            }
            let counts = (0..len).map(|_| next()).collect::<Result<Vec<u64>>>()?;
            let store = Store { offset, counts };
            if sign == 0 {
                s.pos = store;
            } else {
                s.neg = store;
            }
        }
        if !r.is_empty() {
            return Err(bad());
        }
        Ok(s)
    }
}

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn unvarint(r: &mut &[u8]) -> Option<u64> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = r.split_first()?;
        *r = rest;
        n |= u64::from(b & 0x7F) << shift;
        if b < 0x80 {
            return Some(n);
        }
    }
    None
}

fn zigzag(i: i64) -> u64 {
    ((i << 1) ^ (i >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

// ---------------------------------------------------------------- the type

/// The registered type `ddsketch`.
pub struct DdSketchType;

/// The interned `ddsketch` type.
pub fn ddsketch_type() -> TypeRef {
    TypeRef::named("ddsketch").expect("the built-in module registers ddsketch")
}

fn value(s: &DdSketch) -> Value {
    Value::ext(ddsketch_type(), s.to_bytes())
}

fn of_value(v: &Value) -> Result<DdSketch> {
    match v {
        Value::Ext(t, b) if *t == ddsketch_type() => DdSketch::from_bytes(b),
        Value::Bytea(b) => DdSketch::from_bytes(b),
        _ => Err(Error::internal("not a ddsketch value")),
    }
}

impl Type for DdSketchType {
    fn name(&self) -> &str {
        "ddsketch"
    }
    fn category(&self) -> &str {
        "sketch"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let b = crate::bytea::parse(text)?;
        let s = DdSketch::from_bytes(&b)?;
        Ok(value(&s))
    }
    fn output(&self, v: &Value) -> String {
        crate::bytea::format_hex(v.ext_bytes().unwrap_or_default())
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn cast_from(&self, v: &Value) -> Option<Result<Value>> {
        match v {
            Value::Bytea(b) => Some(DdSketch::from_bytes(b).map(|s| value(&s))),
            _ => None,
        }
    }
    fn cast_to(&self, v: &Value, to: DataType) -> Option<Result<Value>> {
        match to {
            DataType::Bytea => Some(Ok(Value::Bytea(v.ext_bytes().unwrap_or_default().into()))),
            _ => None,
        }
    }
    fn examples(&self) -> Vec<Value> {
        let mut s = DdSketch::new(ALPHA).unwrap();
        let mut out = vec![value(&s)];
        for x in [1.0, -2.5, 0.0, 1e300, f64::NAN, f64::NEG_INFINITY, 3e-200] {
            s.add(x);
            out.push(value(&s));
        }
        out
    }
}

// ------------------------------------------------------------- fractions

/// The fractions asked for: one, or an array (its `NULL`s give `NULL`s).
#[derive(Clone, Debug)]
enum Fractions {
    One(Option<f64>),
    Many(Vec<Option<f64>>),
}

fn fraction(v: &Value) -> Result<Option<f64>> {
    let f = match v {
        Value::Null => return Ok(None),
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        Value::Numeric(d) => d.to_f64(),
        _ => return Err(Error::internal("a fraction that is not a number")),
    };
    if !(0.0..=1.0).contains(&f) {
        return Err(
            Error::out_of_range(format!("percentile value {f} is not between 0 and 1"))
                .with_hint("A percentile is a fraction: 0.95, not 95."),
        );
    }
    Ok(Some(f))
}

fn fractions(v: &Value) -> Result<Fractions> {
    Ok(match v {
        Value::Array(fs) => Fractions::Many(fs.iter().map(fraction).collect::<Result<_>>()?),
        v => Fractions::One(fraction(v)?),
    })
}

fn read(s: Option<&DdSketch>, fs: &Fractions) -> Value {
    let at = |f: Option<f64>| match (s, f) {
        (Some(s), Some(f)) => s.quantile(f).map_or(Value::Null, Value::Float),
        _ => Value::Null,
    };
    match fs {
        Fractions::One(f) => at(*f),
        Fractions::Many(fs) => {
            if s.is_none_or(|s| s.count() == 0) {
                Value::Null
            } else {
                Value::Array(fs.iter().map(|&f| at(f)).collect())
            }
        }
    }
}

fn fraction_type(name: &str, a: &ArgInfo) -> Result<(Arg, DataType)> {
    let float_array = DataType::Array(&DataType::Float);
    match a.ty {
        Some(DataType::Array(t)) if t.is_number() => Ok((Arg::Convert(float_array), float_array)),
        Some(t) if t.is_number() => Ok((Arg::Convert(DataType::Float), DataType::Float)),
        None => Ok((Arg::Untyped(DataType::Float), DataType::Float)),
        Some(t) => Err(Error::undefined_function(format!(
            "function {name} with a fraction of type {t} does not exist"
        ))),
    }
}

fn number_arg(name: &str, a: &ArgInfo) -> Result<Arg> {
    match a.ty {
        Some(t) if !t.is_number() => Err(Error::undefined_function(format!(
            "function {name}({t}) does not exist"
        ))),
        _ => Ok(Arg::Convert(DataType::Float)),
    }
}

// ------------------------------------------------------------- aggregates

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `approx_percentile(x, f)`.
    Percentile,
    /// `ddsketch_agg(x [, α])`.
    Agg,
    /// `ddsketch_merge(s)`.
    Merge,
}

struct DdAgg(Kind, &'static str);

/// Registers the type, its aggregates and functions.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(DdSketchType))?;
    for name in ["approx_percentile", "approx_quantile"] {
        r.add_aggregate(&[name], Arc::new(DdAgg(Kind::Percentile, name)))?;
    }
    r.add_aggregate(
        &["ddsketch_agg"],
        Arc::new(DdAgg(Kind::Agg, "ddsketch_agg")),
    )?;
    r.add_aggregate(
        &["ddsketch_merge"],
        Arc::new(DdAgg(Kind::Merge, "ddsketch_merge")),
    )?;
    r.add_function(&["ddsketch_quantile"], Arc::new(DdRead(true)))?;
    r.add_function(&["ddsketch_count"], Arc::new(DdRead(false)))?;
    Ok(())
}

fn sketch_arg(name: &str, a: &ArgInfo) -> Result<Arg> {
    if a.ty.is_some_and(|t| t != DataType::Ext(ddsketch_type())) {
        return Err(Error::undefined_function(format!(
            "function {name}({}) does not exist",
            a.ty.unwrap()
        ))
        .with_hint(format!("{name} takes the sketches ddsketch_agg makes.")));
    }
    Ok(Arg::Untyped(DataType::Ext(ddsketch_type())))
}

impl Aggregate for DdAgg {
    fn name(&self) -> &str {
        self.1
    }
    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            Kind::Percentile => 2..=2,
            Kind::Agg => 1..=2,
            Kind::Merge => 1..=1,
        }
    }
    fn order_free(&self) -> bool {
        true
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let sketch = DataType::Ext(ddsketch_type());
        Ok(match self.0 {
            Kind::Percentile => {
                let (f, returns) = fraction_type(self.1, &args[1])?;
                Signature::new(
                    vec![number_arg(self.1, &args[0])?, f],
                    Returns::Type(Some(returns)),
                )
            }
            Kind::Agg => {
                let mut a = vec![number_arg(self.1, &args[0])?];
                if args.len() == 2 {
                    a.push(number_arg(self.1, &args[1])?);
                }
                Signature::new(a, Returns::Type(Some(sketch)))
            }
            Kind::Merge => Signature::new(
                vec![sketch_arg(self.1, &args[0])?],
                Returns::Type(Some(sketch)),
            ),
        })
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One(Groups::new(self.0)))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups::new(self.0)))
    }
}

/// A group's sketch and what it was asked (from its first row).
struct Group {
    sketch: DdSketch,
    fractions: Fractions,
}

struct Groups {
    kind: Kind,
    groups: Vec<Option<Group>>,
}

impl Groups {
    fn new(kind: Kind) -> Groups {
        Groups {
            kind,
            groups: Vec::new(),
        }
    }

    /// Group `g`, started from its first row's other arguments.
    fn group(&mut self, g: usize, rest: impl FnOnce() -> Result<Value>) -> Result<&mut Group> {
        if self.groups[g].is_none() {
            let (alpha, fractions) = match self.kind {
                Kind::Percentile => (ALPHA, fractions(&rest()?)?),
                Kind::Agg => match rest()? {
                    Value::Null => (ALPHA, Fractions::One(None)),
                    Value::Float(a) => (a, Fractions::One(None)),
                    _ => return Err(Error::internal("ddsketch_agg's accuracy")),
                },
                Kind::Merge => (ALPHA, Fractions::One(None)),
            };
            self.groups[g] = Some(Group {
                sketch: DdSketch::new(alpha)?,
                fractions,
            });
        }
        Ok(self.groups[g].as_mut().unwrap())
    }

    fn add(&mut self, g: usize, x: &Value, rest: impl FnOnce() -> Result<Value>) -> Result<()> {
        if self.kind == Kind::Merge {
            let other = of_value(x)?;
            match &mut self.groups[g] {
                Some(grp) => grp.sketch.merge(&other)?,
                None => {
                    self.groups[g] = Some(Group {
                        sketch: other,
                        fractions: Fractions::One(None),
                    })
                }
            }
            return Ok(());
        }
        let Value::Float(x) = x else {
            return Err(Error::internal("a sketch's value that is not a double"));
        };
        self.group(g, rest)?.sketch.add(*x);
        Ok(())
    }

    fn result(&self, g: usize) -> Value {
        match (self.kind, &self.groups[g]) {
            (Kind::Percentile, Some(grp)) => read(Some(&grp.sketch), &grp.fractions),
            (Kind::Percentile, None) | (_, None) => Value::Null,
            (_, Some(grp)) => value(&grp.sketch),
        }
    }
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        if groups > self.groups.len() {
            self.groups.resize_with(groups, || None);
        }
    }

    fn group_bytes(&self) -> usize {
        256
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let rest = |r: usize| move || Ok(args.get(1).map_or(Value::Null, |a| a.value(r)));
        for (&r, &g) in rows.iter().zip(groups) {
            let (r, g) = (r as usize, g as usize);
            if let (Vector::Float(d, _), Kind::Percentile | Kind::Agg) = (args[0], self.kind) {
                self.group(g, rest(r))?.sketch.add(d[r]);
                continue;
            }
            self.add(g, &args[0].value(r), rest(r))?;
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(match &self.groups[group] {
            None => Value::Null,
            Some(grp) => {
                let fs = match &grp.fractions {
                    Fractions::One(f) => Value::Float(f.unwrap_or(f64::NAN)),
                    Fractions::Many(fs) => Value::Array(
                        fs.iter()
                            .map(|f| f.map_or(Value::Null, Value::Float))
                            .collect(),
                    ),
                };
                let null_one = matches!(grp.fractions, Fractions::One(None));
                Value::Array(vec![
                    if null_one { Value::Null } else { fs },
                    Value::Bytea(grp.sketch.to_bytes().into()),
                ])
            }
        })
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(parts) = partial else {
            return Ok(());
        };
        let [fs, Value::Bytea(b)] = &parts[..] else {
            return Err(Error::internal("a ddsketch's partial state"));
        };
        let other = DdSketch::from_bytes(b)?;
        match &mut self.groups[group] {
            Some(grp) => grp.sketch.merge(&other)?,
            None => {
                self.groups[group] = Some(Group {
                    sketch: other,
                    fractions: fractions(fs)?,
                })
            }
        }
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        Ok(self.result(group))
    }

    fn finish_keeps_state(&self) -> bool {
        true
    }
}

/// One group's (the row-at-a-time executor's).
struct One(Groups);

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.0.resize(1);
        if args[0].is_null() {
            return Ok(());
        }
        let rest = args.get(1).cloned().unwrap_or(Value::Null);
        self.0.add(0, &args[0], || Ok(rest))
    }
    fn finish(&mut self) -> Result<Value> {
        self.0.resize(1);
        Ok(self.0.result(0))
    }
}

/// `ddsketch_quantile(s, f)` (`true`) and `ddsketch_count(s)`.
struct DdRead(bool);

impl ScalarFunction for DdRead {
    fn name(&self) -> &str {
        if self.0 {
            "ddsketch_quantile"
        } else {
            "ddsketch_count"
        }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        if self.0 { 2..=2 } else { 1..=1 }
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let s = sketch_arg(self.name(), &args[0])?;
        Ok(if self.0 {
            let (f, returns) = fraction_type(self.name(), &args[1])?;
            Signature::new(vec![s, f], Returns::Type(Some(returns)))
        } else {
            Signature::new(vec![s], Returns::Type(Some(DataType::BigInt)))
        })
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let s = of_value(&args[0])?;
        Ok(if self.0 {
            read(Some(&s), &fractions(&args[1])?)
        } else {
            Value::Int(s.count() as i64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted_index(v: &[f64], f: f64) -> f64 {
        v[(f * (v.len() - 1) as f64).floor() as usize]
    }

    #[test]
    fn quantiles_are_within_the_relative_error() {
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for alpha in [0.01, 0.05, 0.001] {
            let mut v = Vec::new();
            let mut s = DdSketch::new(alpha).unwrap();
            for _ in 0..50_000 {
                // Signs, and magnitudes over as many decades as the buckets hold.
                let r = next();
                // 2048 buckets a sign reach 17.6 decades at α = 1 %, a
                // tenth of that at 0.1 %.
                let decades = (17.0 * alpha / 0.01).min(17.0);
                let u = (r >> 1) % 1_000_000;
                let mag = 10f64.powf(decades * (u as f64 / 1e6 - 0.5));
                let x = if r >> 63 == 1 { -mag } else { mag };
                v.push(x);
                s.add(x);
            }
            v.sort_by(f64::total_cmp);
            for f in [0.0, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 0.999, 1.0] {
                let want = sorted_index(&v, f);
                let got = s.quantile(f).unwrap();
                assert!(
                    (got - want).abs() <= alpha * want.abs() * (1.0 + 1e-12),
                    "α {alpha} f {f}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn mapping_is_monotone_and_its_inverse_bounds_each_bucket() {
        let s = DdSketch::new(ALPHA).unwrap();
        let mut x = f64::MIN_POSITIVE;
        let mut last = i64::MIN;
        while x < 1e300 {
            let i = s.index(x);
            assert!(i >= last);
            last = i;
            let lo = exp2ish(i as f64 / s.multiplier);
            let hi = exp2ish((i + 1) as f64 / s.multiplier);
            assert!(
                lo <= x * (1.0 + 1e-12) && x <= hi * (1.0 + 1e-12),
                "{x}: [{lo}, {hi})"
            );
            assert!(
                hi / lo <= (1.0 + ALPHA) / (1.0 - ALPHA) * (1.0 + 1e-12),
                "{x}: bucket {i} [{lo}, {hi}) ratio {}",
                hi / lo
            );
            x *= 1.37;
        }
    }

    #[test]
    fn a_sketch_is_its_values_whatever_the_order() {
        // Wide enough to collapse: 2048 buckets cover about 2⁵⁸.
        let xs: Vec<f64> = (0..20_000)
            .map(|i| {
                let e = (i * 7919) % 400 - 200;
                (1.0 + (i % 97) as f64 / 97.0) * 2f64.powi(e) * if i % 3 == 0 { -1.0 } else { 1.0 }
            })
            .collect();
        let mut whole = DdSketch::new(ALPHA).unwrap();
        xs.iter().for_each(|&x| whole.add(x));
        let mut rev = DdSketch::new(ALPHA).unwrap();
        xs.iter().rev().for_each(|&x| rev.add(x));
        assert_eq!(whole.to_bytes(), rev.to_bytes());
        for cut in [1, 777, 10_000, 19_999] {
            let mut a = DdSketch::new(ALPHA).unwrap();
            let mut b = DdSketch::new(ALPHA).unwrap();
            xs[..cut].iter().for_each(|&x| a.add(x));
            xs[cut..].iter().for_each(|&x| b.add(x));
            let mut ab = a.clone();
            ab.merge(&b).unwrap();
            b.merge(&a).unwrap();
            assert_eq!(ab.to_bytes(), whole.to_bytes(), "cut {cut}");
            assert_eq!(b.to_bytes(), whole.to_bytes(), "cut {cut}, reversed");
        }
        assert!(whole.pos.counts.len() as i64 <= MAX_BINS);
        assert_eq!(
            DdSketch::from_bytes(&whole.to_bytes()).unwrap().to_bytes(),
            whole.to_bytes()
        );
    }

    #[test]
    fn edges_order_as_sql() {
        let mut s = DdSketch::new(ALPHA).unwrap();
        for x in [
            f64::NAN,
            f64::INFINITY,
            -f64::INFINITY,
            0.0,
            -0.0,
            1.0,
            -1.0,
            5e-324,
        ] {
            s.add(x);
        }
        // Ranks 0 … 7: −∞, −1, 0 ×3 (−0, 0, a subnormal), 1, ∞, NaN.
        let at = |rank: f64| s.quantile(rank / 7.0).unwrap();
        let near = |got: f64, want: f64| (got - want).abs() <= ALPHA * want.abs() * (1.0 + 1e-9);
        assert_eq!(at(0.0), f64::NEG_INFINITY);
        assert!(near(at(1.5), -1.0));
        assert_eq!(at(3.5), 0.0);
        assert!(near(at(5.5), 1.0));
        assert_eq!(at(6.5), f64::INFINITY);
        assert!(s.quantile(1.0).unwrap().is_nan());
        assert_eq!(DdSketch::new(ALPHA).unwrap().quantile(0.5), None);
        assert!((ln_gamma(0.01) - (1.01f64 / 0.99).ln()).abs() < 1e-15);
    }
}
