//! Approximate distinct counts (PLAN.md §M14.27 S6): HyperLogLog++
//! (Heule, Nunkesser, Hall, EDBT 2013) over [`crate::stablehash`], read
//! by Ertl's improved estimator ("New cardinality estimation algorithms
//! for HyperLogLog sketches", 2017).
//!
//! A sketch starts **sparse**: the set of its hashes' 25-bit prefixes,
//! each with the rank (`1 +` leading zeros) of the bits after it — near
//! exact for small counts.  Past 4096 of them (the 16 KiB a dense sketch
//! takes) it becomes **dense**: 2¹⁴ registers, each the greatest rank of
//! the hashes whose first 14 bits name it — a standard error of
//! 1.04 / √2¹⁴ = 0.81 %.  Both merge by maximum, and the change of form
//! depends only on the set of prefixes, so a sketch — and its estimate —
//! is a function of the values it saw, whatever their order or the
//! workers that saw them.
//!
//! The estimator (Ertl's, no empirical bias tables) uses only `+`, `*`,
//! `/` and `√`, which IEEE 754 rounds the same everywhere: an estimate is
//! the same bits on every machine.
//!
//! `approx_count_distinct(x)`; `hll_agg(x)` the sketch as a value of the
//! registered type `hll` (canonical bytes: version, precision, form, then
//! the sorted prefixes or the registers), `hll_merge(h)` the union of
//! sketches, `hll_count(h)` its estimate — so a rollup keeps a day's
//! sketch and a month is merged from them.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, CanonicalBytes, Equality, GroupAccumulator,
    Hashing, Registry, Returns, ScalarFunction, Signature, Type, TypeRef, Volatility,
};
use crate::stablehash::{hash_float, hash_int, hash_value};
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// The dense form's precision: 2¹⁴ registers.
const P: u32 = 14;
const M: usize = 1 << P;
/// The sparse form's precision.
const SP: u32 = 25;
/// Prefixes a sparse sketch holds at most (its 4 bytes each: a dense
/// sketch's size).
const SPARSE_MAX: usize = M / 4;
/// Prefixes gathered before they are merged into the sorted set.
const BUFFER: usize = 1024;
/// The format's version.
const VERSION: u8 = 1;
/// `1 / (2 ln 2)`.
const ALPHA_INF: f64 = 0.721_347_520_444_481_7;

/// A sketch.
#[derive(Clone, Debug, Default)]
pub struct Hll {
    /// Sparse: sorted, one entry a prefix — `prefix << 6 | rank`.
    sparse: Vec<u32>,
    /// Sparse entries not yet merged into `sparse`.
    buf: Vec<u32>,
    /// Dense: the registers (then `sparse` and `buf` are empty).
    dense: Option<Box<[u8]>>,
}

/// A sparse entry's prefix.
#[inline]
fn prefix(e: u32) -> u32 {
    e >> 6
}

#[inline]
fn sparse_entry(h: u64) -> u32 {
    let idx = (h >> (64 - SP)) as u32;
    let rank = (h << SP).leading_zeros().min(64 - SP) + 1;
    idx << 6 | rank
}

/// A dense register and its rank from a sparse entry: the bits between
/// the two precisions decide, else the sparse rank past them.
#[inline]
fn dense_of(e: u32) -> (usize, u8) {
    let idx = prefix(e);
    let reg = (idx >> (SP - P)) as usize;
    let mid = idx & ((1 << (SP - P)) - 1);
    let rank = if mid != 0 {
        mid.leading_zeros() - (32 - (SP - P)) + 1
    } else {
        (SP - P) + (e & 63)
    };
    (reg, rank as u8)
}

impl Hll {
    /// An empty sketch.
    pub fn new() -> Hll {
        Hll::default()
    }

    /// Adds a hash.
    #[inline]
    pub fn add_hash(&mut self, h: u64) {
        match &mut self.dense {
            Some(regs) => {
                let reg = (h >> (64 - P)) as usize;
                let rank = ((h << P).leading_zeros().min(64 - P) + 1) as u8;
                if regs[reg] < rank {
                    regs[reg] = rank;
                }
            }
            None => {
                self.buf.push(sparse_entry(h));
                if self.buf.len() >= BUFFER {
                    self.flush();
                }
            }
        }
    }

    /// Adds a sparse entry — to the registers if the sketch has become
    /// dense meanwhile.
    fn add_entry(&mut self, e: u32) {
        match &mut self.dense {
            Some(regs) => {
                let (reg, rank) = dense_of(e);
                if regs[reg] < rank {
                    regs[reg] = rank;
                }
            }
            None => {
                self.buf.push(e);
                if self.buf.len() >= BUFFER {
                    self.flush();
                }
            }
        }
    }

    /// The buffer merged into the sorted set (the greatest rank per
    /// prefix), and the sketch made dense if the set has grown past
    /// [`SPARSE_MAX`].
    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        self.buf.sort_unstable();
        let mut out = Vec::with_capacity(self.sparse.len() + self.buf.len());
        let (mut a, mut b) = (self.sparse.iter().peekable(), self.buf.iter().peekable());
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(&&x), Some(&&y)) => {
                    if x <= y {
                        a.next();
                        x
                    } else {
                        b.next();
                        y
                    }
                }
                (Some(&&x), None) => {
                    a.next();
                    x
                }
                (None, Some(&&y)) => {
                    b.next();
                    y
                }
                (None, None) => break,
            };
            // Entries ascend, so the last of a prefix has its greatest rank.
            match out.last_mut() {
                Some(last) if prefix(*last) == prefix(next) => *last = next,
                _ => out.push(next),
            }
        }
        self.buf.clear();
        self.sparse = out;
        if self.sparse.len() > SPARSE_MAX {
            self.densify();
        }
    }

    fn densify(&mut self) {
        let mut regs = vec![0u8; M].into_boxed_slice();
        for &e in self.sparse.iter().chain(&self.buf) {
            let (reg, rank) = dense_of(e);
            if regs[reg] < rank {
                regs[reg] = rank;
            }
        }
        self.sparse = Vec::new();
        self.buf = Vec::new();
        self.dense = Some(regs);
    }

    /// Adds `other`'s values (the union).
    pub fn merge(&mut self, other: &Hll) {
        match (&mut self.dense, &other.dense) {
            (Some(a), Some(b)) => {
                for (x, &y) in a.iter_mut().zip(b.iter()) {
                    *x = (*x).max(y);
                }
            }
            (None, Some(b)) => {
                let mut b = Hll {
                    dense: Some(b.clone()),
                    ..Hll::default()
                };
                let mine = std::mem::take(self);
                for &e in mine.sparse.iter().chain(&mine.buf) {
                    let (reg, rank) = dense_of(e);
                    let regs = b.dense.as_mut().unwrap();
                    if regs[reg] < rank {
                        regs[reg] = rank;
                    }
                }
                *self = b;
            }
            (Some(a), None) => {
                for &e in other.sparse.iter().chain(&other.buf) {
                    let (reg, rank) = dense_of(e);
                    if a[reg] < rank {
                        a[reg] = rank;
                    }
                }
            }
            (None, None) => {
                for &e in other.sparse.iter().chain(&other.buf) {
                    self.add_entry(e);
                }
            }
        }
    }

    /// The estimated number of distinct values.
    pub fn estimate(&mut self) -> f64 {
        self.flush();
        match &self.dense {
            Some(regs) => {
                let mut counts = [0u32; 64 - P as usize + 2];
                for &r in regs.iter() {
                    counts[r as usize] += 1;
                }
                ertl(&counts, M as f64)
            }
            None => {
                // Sparse: 2²⁵ registers, of which the set's are not zero.
                let mut counts = [0u32; 64 - SP as usize + 2];
                for &e in &self.sparse {
                    counts[(e & 63) as usize] += 1;
                }
                counts[0] = (1u32 << SP) - self.sparse.len() as u32;
                ertl(&counts, (1u64 << SP) as f64)
            }
        }
    }

    /// The canonical bytes: version, precision, form (0 sparse, 1 dense),
    /// then the sorted entries (`u32` little-endian) or the registers.
    pub fn to_bytes(&mut self) -> Vec<u8> {
        self.flush();
        let mut b = vec![VERSION, P as u8];
        match &self.dense {
            Some(regs) => {
                b.push(1);
                b.extend_from_slice(regs);
            }
            None => {
                b.push(0);
                for e in &self.sparse {
                    b.extend_from_slice(&e.to_le_bytes());
                }
            }
        }
        b
    }

    /// A sketch from its canonical bytes.
    pub fn from_bytes(b: &[u8]) -> Result<Hll> {
        let bad = || Error::invalid_parameter("not a hll sketch");
        match b {
            [VERSION, p, 1, regs @ ..] if u32::from(*p) == P && regs.len() == M => {
                if regs.iter().any(|&r| u32::from(r) > 64 - P + 1) {
                    return Err(bad());
                }
                Ok(Hll {
                    dense: Some(regs.into()),
                    ..Hll::default()
                })
            }
            [VERSION, p, 0, rest @ ..] if u32::from(*p) == P && rest.len() % 4 == 0 => {
                let sparse: Vec<u32> = rest
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_le_bytes(*c))
                    .collect();
                let valid = sparse.len() <= SPARSE_MAX
                    && sparse.windows(2).all(|w| prefix(w[0]) < prefix(w[1]))
                    && sparse
                        .iter()
                        .all(|&e| (1..=64 - SP + 1).contains(&(e & 63)) && prefix(e) < 1 << SP);
                if !valid {
                    return Err(bad());
                }
                Ok(Hll {
                    sparse,
                    ..Hll::default()
                })
            }
            [v, ..] if *v != VERSION => Err(Error::feature(format!(
                "hll sketch format version {v} (this version reads {VERSION})"
            ))),
            _ => Err(bad()),
        }
    }
}

/// Ertl's improved raw estimator over `m` registers, `counts[k]` of them
/// holding `k` (`k` up to `q + 1`).
fn ertl(counts: &[u32], m: f64) -> f64 {
    let q = counts.len() - 2;
    if f64::from(counts[0]) == m {
        return 0.0;
    }
    let mut z = m * tau(1.0 - f64::from(counts[q + 1]) / m);
    for k in (1..=q).rev() {
        z = 0.5 * (z + f64::from(counts[k]));
    }
    z += m * sigma(f64::from(counts[0]) / m);
    ALPHA_INF * m * m / z
}

fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if z == before {
            return z / 3.0;
        }
    }
}

// ---------------------------------------------------------------- the type

/// The registered type `hll`.
pub struct HllType;

/// The interned `hll` type.
pub fn hll_type() -> TypeRef {
    TypeRef::named("hll").expect("the built-in module registers hll")
}

fn value(h: &mut Hll) -> Value {
    Value::ext(hll_type(), h.to_bytes())
}

fn of_value(v: &Value) -> Result<Hll> {
    match v {
        Value::Ext(t, b) if *t == hll_type() => Hll::from_bytes(b),
        Value::Bytea(b) => Hll::from_bytes(b),
        _ => Err(Error::internal("not a hll value")),
    }
}

impl Type for HllType {
    fn name(&self) -> &str {
        "hll"
    }
    fn category(&self) -> &str {
        "sketch"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let b = crate::bytea::parse(text)?;
        Hll::from_bytes(&b)?;
        Ok(Value::ext(hll_type(), b))
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
            Value::Bytea(b) => Some(Hll::from_bytes(b).map(|mut h| value(&mut h))),
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
        let mut out = vec![value(&mut Hll::new())];
        let mut h = Hll::new();
        for i in 0..10 {
            h.add_hash(hash_int(i));
        }
        out.push(value(&mut h));
        for i in 10..6000 {
            h.add_hash(hash_int(i));
        }
        out.push(value(&mut h));
        out
    }
}

// ------------------------------------------------------------- aggregates

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `approx_count_distinct(x)`: the estimate.
    Count,
    /// `hll_agg(x)`: the sketch.
    Agg,
    /// `hll_merge(h)`: the union of sketches.
    Merge,
}

struct HllAgg(Kind, &'static str);

/// Registers the type, its aggregates and functions.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(HllType))?;
    r.add_aggregate(
        &["approx_count_distinct"],
        Arc::new(HllAgg(Kind::Count, "approx_count_distinct")),
    )?;
    r.add_aggregate(&["hll_agg"], Arc::new(HllAgg(Kind::Agg, "hll_agg")))?;
    r.add_aggregate(&["hll_merge"], Arc::new(HllAgg(Kind::Merge, "hll_merge")))?;
    r.add_function(&["hll_count", "hll_cardinality"], Arc::new(HllCount))?;
    Ok(())
}

impl Aggregate for HllAgg {
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
        let arg = match self.0 {
            Kind::Merge => {
                if args[0].ty.is_some_and(|t| t != DataType::Ext(hll_type())) {
                    return Err(Error::undefined_function(format!(
                        "function hll_merge({}) does not exist",
                        args[0].ty.unwrap()
                    ))
                    .with_hint("hll_merge takes the sketches hll_agg makes."));
                }
                Arg::Untyped(DataType::Ext(hll_type()))
            }
            _ => Arg::Untyped(DataType::Text),
        };
        let returns = match self.0 {
            Kind::Count => DataType::BigInt,
            _ => DataType::Ext(hll_type()),
        };
        Ok(Signature::new(vec![arg], Returns::Type(Some(returns))))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One(Groups::new(self.0)))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups::new(self.0)))
    }
}

struct Groups {
    kind: Kind,
    sketches: Vec<Option<Hll>>,
}

impl Groups {
    fn new(kind: Kind) -> Groups {
        Groups {
            kind,
            sketches: Vec::new(),
        }
    }

    fn sketch(&mut self, g: usize) -> &mut Hll {
        self.sketches[g].get_or_insert_with(Hll::new)
    }

    fn add(&mut self, g: usize, v: &Value) -> Result<()> {
        if self.kind == Kind::Merge {
            let other = of_value(v)?;
            self.sketch(g).merge(&other);
        } else {
            self.sketch(g).add_hash(hash_value(v));
        }
        Ok(())
    }

    fn result(&mut self, g: usize) -> Value {
        match (self.kind, &mut self.sketches[g]) {
            (Kind::Count, None) => Value::Int(0),
            (Kind::Count, Some(h)) => Value::Int(h.estimate().round() as i64),
            (_, None) => Value::Null,
            (_, Some(h)) => value(h),
        }
    }
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        if groups > self.sketches.len() {
            self.sketches.resize_with(groups, || None);
        }
    }

    fn group_bytes(&self) -> usize {
        // A sketch starts small and grows to 16 KiB; most groups of many
        // stay sparse.
        256
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let pairs = rows
            .iter()
            .zip(groups)
            .map(|(&r, &g)| (r as usize, g as usize));
        match (self.kind, args[0]) {
            (Kind::Merge, a) => {
                for (r, g) in pairs {
                    self.add(g, &a.value(r))?;
                }
            }
            (_, Vector::Int(d, _)) => {
                for (r, g) in pairs {
                    self.sketch(g).add_hash(hash_int(d[r]));
                }
            }
            (_, Vector::Float(d, _)) => {
                for (r, g) in pairs {
                    self.sketch(g).add_hash(hash_float(d[r]));
                }
            }
            (_, a) => {
                for (r, g) in pairs {
                    self.add(g, &a.value(r))?;
                }
            }
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(match &mut self.sketches[group] {
            None => Value::Null,
            Some(h) => Value::Bytea(h.to_bytes().into()),
        })
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        if let Value::Bytea(b) = partial {
            let other = Hll::from_bytes(&b)?;
            self.sketch(group).merge(&other);
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
        if !args[0].is_null() {
            self.0.add(0, &args[0])?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        self.0.resize(1);
        Ok(self.0.result(0))
    }
}

/// `hll_count(h)`: a sketch's estimate.
struct HllCount;

impl ScalarFunction for HllCount {
    fn name(&self) -> &str {
        "hll_count"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if args[0].ty.is_some_and(|t| t != DataType::Ext(hll_type())) {
            return Err(Error::undefined_function(format!(
                "function hll_count({}) does not exist",
                args[0].ty.unwrap()
            )));
        }
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Ext(hll_type()))],
            Returns::Type(Some(DataType::BigInt)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        Ok(Value::Int(of_value(&args[0])?.estimate().round() as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(range: std::ops::Range<i64>) -> Hll {
        let mut h = Hll::new();
        for i in range {
            h.add_hash(hash_int(i));
        }
        h
    }

    #[test]
    fn estimates_are_within_the_error() {
        for n in [
            0i64, 1, 2, 10, 100, 1000, 4000, 5000, 20_000, 100_000, 1_000_000,
        ] {
            let e = of(0..n).estimate();
            let err = (e - n as f64).abs() / (n.max(1) as f64);
            // Sparse: near exact; dense: 3 standard errors.
            let bound = if n <= 4000 { 0.002 } else { 3.0 * 0.0081 };
            assert!(err <= bound, "n {n}: {e} ({err})");
        }
    }

    #[test]
    fn a_sketch_is_its_set_whatever_the_order() {
        let n = 30_000;
        let whole = of(0..n).to_bytes();
        // In pieces, merged in several orders; sparse into dense and back.
        for cuts in [
            vec![0, 10, 5000, 29_000, n],
            vec![0, 4000, 4100, n],
            vec![0, n / 2, n],
        ] {
            let parts: Vec<Hll> = cuts.windows(2).map(|w| of(w[0]..w[1])).collect();
            let mut fwd = Hll::new();
            for p in &parts {
                fwd.merge(p);
            }
            let mut back = Hll::new();
            for p in parts.iter().rev() {
                back.merge(p);
            }
            assert_eq!(fwd.to_bytes(), whole);
            assert_eq!(back.to_bytes(), whole);
        }
        // Sparse sketches merged one into another until it turns dense
        // part way through a merge.
        let mut many = Hll::new();
        for c in (0..n).collect::<Vec<_>>().chunks(3000) {
            many.merge(&of(c[0]..c[c.len() - 1] + 1));
        }
        assert_eq!(many.to_bytes(), whole);
        // Small: stays sparse, the same set from any order.
        let mut a = of(0..300);
        let mut b = of(150..300);
        b.merge(&of(0..150));
        assert_eq!(a.to_bytes(), b.to_bytes());
        assert_eq!(a.to_bytes()[2], 0);
    }

    #[test]
    fn sparse_and_dense_ranks_agree() {
        // A hash's register and rank, through the sparse entry or directly.
        for h in [
            0u64,
            u64::MAX,
            1,
            1 << 50,
            1 << 39,
            1 << 38,
            0x0003_FFFF_0000_0001,
            0xDEAD_BEEF_0000_0000,
        ] {
            let mut d = Hll::new();
            d.densify();
            d.add_hash(h);
            let (reg, rank) = dense_of(sparse_entry(h));
            assert_eq!(d.dense.as_ref().unwrap()[reg], rank, "{h:#x}");
        }
    }

    #[test]
    fn bytes_round_trip_and_bad_ones_are_refused() {
        for n in [0, 5, 5000] {
            let mut h = of(0..n);
            let b = h.to_bytes();
            assert_eq!(Hll::from_bytes(&b).unwrap().to_bytes(), b);
        }
        assert!(Hll::from_bytes(&[]).is_err());
        assert!(Hll::from_bytes(&[1, 14, 0, 1, 2, 3]).is_err());
        assert_eq!(Hll::from_bytes(&[2, 14, 0]).unwrap_err().code, "0A000");
    }
}
