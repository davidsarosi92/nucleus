//! Sampling and splits (PLAN.md §M14.27 S7).
//!
//! **`TABLESAMPLE BERNOULLI (p) | SYSTEM (p) [REPEATABLE (seed)]`**
//! (PostgreSQL's) — the binder puts a filter over the table's scan.  With
//! `REPEATABLE`, a row is in the sample when its primary key's (or, with
//! none, its columns') key encoding hashes ([`crate::stablehash`], the
//! seed's bits as xxHash64's seed) below `p / 100` of the hash space: the
//! same rows on every run, worker count, version and machine, in row and
//! column storage alike, and a smaller `p`'s sample within a larger one's.
//! Without it, each row is taken with probability `p / 100` from a
//! per-thread generator seeded from the database's random source — a new
//! sample each run, as PostgreSQL's.  `SYSTEM` samples rows the same way:
//! there are no blocks a plan skips yet.
//!
//! **`hash_unit(key [, seed])`** — the same hash as a double in `[0, 1)`,
//! so `WHERE hash_unit(id) < 0.8` is the same training set everywhere.
//! **`random()`** — PostgreSQL's, from the same per-thread generator.
//!
//! **`reservoir_sample(x, n [, seed])`** — `n` of the values, each set of
//! `n` equally likely: Li's Algorithm L ([`Reservoir`], `ANALYZE`'s too)
//! from a generator seeded by `seed` (0 by default), so the same sample
//! every run of a plan (its gaps computed with `ln` and `exp`, whose last
//! bit is the platform's, so on another machine almost always).  Two
//! reservoirs merge by drawing, without replacement, how many values come
//! from each (hypergeometrically, from their row counts) out of their
//! shuffled samples — uniform again.  The draws depend on the rows'
//! order, so the aggregate is not order-free: its morsels merge in their
//! order (`ORDER BY hash_unit(id) LIMIT n` is a sample independent of the
//! plan).

use std::cell::Cell;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, GroupAccumulator, Registry, Returns,
    ScalarFunction, Signature, Volatility,
};
use crate::stablehash::xxh64;
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// Registers the sampling functions.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["hash_unit"], Arc::new(HashUnit))?;
    r.add_function(&["random"], Arc::new(Random))?;
    r.add_function(&["__tablesample"], Arc::new(TableSample(false)))?;
    r.add_function(&["__tablesample_repeatable"], Arc::new(TableSample(true)))?;
    r.add_aggregate(&["reservoir_sample"], Arc::new(ReservoirSample))?;
    Ok(())
}

/// A double in `[0, 1)` from a hash's top 53 bits.
fn unit(h: u64) -> f64 {
    (h >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// The bits a seed gives xxHash64 (`-0` as `0`).
fn seed_bits(seed: f64) -> u64 {
    if seed == 0.0 { 0 } else { seed.to_bits() }
}

fn key_bytes(values: &[Value]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 * values.len());
    for v in values {
        crate::encoding::encode_key_value(&mut b, v);
    }
    b
}

// -------------------------------------------------------------- hash_unit

struct HashUnit;

impl ScalarFunction for HashUnit {
    fn name(&self) -> &str {
        "hash_unit"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=2
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut a = vec![Arg::Untyped(DataType::Text)];
        if args.len() == 2 {
            if args[1].ty.is_some_and(|t| !t.is_integer()) {
                return Err(Error::undefined_function(format!(
                    "function hash_unit({}, {}) does not exist",
                    args[0].ty.map_or("unknown".into(), |t| t.to_string()),
                    args[1].ty.unwrap()
                ))
                .with_hint("The seed is an integer."));
            }
            a.push(Arg::Convert(DataType::BigInt));
        }
        Ok(Signature::new(a, Returns::Type(Some(DataType::Float))))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let seed = match args.get(1) {
            Some(Value::Int(s)) => *s as u64,
            _ => 0,
        };
        Ok(Value::Float(unit(match seed {
            0 => crate::stablehash::hash_value(&args[0]),
            _ => xxh64(&key_bytes(&args[..1]), seed),
        })))
    }
}

// ----------------------------------------------------------------- random

/// `random()`: a double in `[0, 1)` from the thread's generator (the one
/// `TABLESAMPLE` without `REPEATABLE` draws from) — volatile, but its
/// calls do not depend on one another, so it runs on the workers.
struct Random;

impl ScalarFunction for Random {
    fn name(&self) -> &str {
        "random"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        0..=0
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            Vec::new(),
            Returns::Type(Some(DataType::Float)),
        ))
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn call(&self, cx: &CallContext<'_>, _: &[Value]) -> Result<Value> {
        Ok(Value::Float(unit(next_random(cx)?)))
    }
}

// ------------------------------------------------------------ TABLESAMPLE

/// `__tablesample(p)` (`false`: a row with probability `p / 100`) and
/// `__tablesample_repeatable(p, seed, key…)` (`true`: by the key's hash).
struct TableSample(bool);

fn percentage(v: &Value) -> Result<f64> {
    let p = match v {
        Value::Float(p) => *p,
        Value::Null => {
            return Err(Error::invalid_tablesample(
                "TABLESAMPLE parameter cannot be null",
            ));
        }
        _ => return Err(Error::internal("a TABLESAMPLE percentage not a double")),
    };
    if !(0.0..=100.0).contains(&p) {
        return Err(Error::invalid_tablesample(
            "sample percentage must be between 0 and 100",
        ));
    }
    Ok(p)
}

thread_local! {
    /// A thread's generator for samples without `REPEATABLE` (0: not
    /// seeded yet).
    static RNG: Cell<u64> = const { Cell::new(0) };
}

/// SplitMix64: [`mix`](crate::kernels::mix) over a counter.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    crate::kernels::mix(*state)
}

fn next_random(cx: &CallContext<'_>) -> Result<u64> {
    RNG.with(|r| {
        let mut s = r.get();
        if s == 0 {
            let mut b = [0u8; 8];
            cx.random(&mut b)?;
            s = u64::from_le_bytes(b) | 1;
        }
        let x = splitmix(&mut s);
        r.set(s);
        Ok(x)
    })
}

impl ScalarFunction for TableSample {
    fn name(&self) -> &str {
        if self.0 {
            "__tablesample_repeatable"
        } else {
            "__tablesample"
        }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        if self.0 { 3..=usize::MAX } else { 1..=1 }
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut a = vec![Arg::Convert(DataType::Float)];
        if self.0 {
            a.push(Arg::Convert(DataType::Float));
            a.extend(args[2..].iter().map(|_| Arg::Keep));
        }
        Ok(Signature::new(a, Returns::Type(Some(DataType::Bool))))
    }
    fn strict(&self) -> bool {
        false
    }
    fn volatility(&self) -> Volatility {
        if self.0 {
            Volatility::Immutable
        } else {
            Volatility::Volatile
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let p = percentage(&args[0])?;
        let u = if self.0 {
            let seed = match &args[1] {
                Value::Float(s) => seed_bits(*s),
                Value::Null => {
                    return Err(Error::invalid_tablesample_repeat(
                        "TABLESAMPLE REPEATABLE parameter cannot be null",
                    ));
                }
                _ => return Err(Error::internal("a TABLESAMPLE seed not a double")),
            };
            unit(xxh64(&key_bytes(&args[2..]), seed))
        } else {
            unit(next_random(cx)?)
        };
        Ok(Value::Bool(u < p / 100.0))
    }
}

// -------------------------------------------------------- reservoir_sample

/// Reservoir sampling by Li's Algorithm L ("Reservoir-sampling algorithms
/// of time complexity O(n(1 + log(N/n)))", ACM TOMS 1994): after the
/// reservoir's first `size` items, the gap to the next item taken is drawn
/// at once (geometric, its parameter shrinking as the stream grows), so a
/// stream of N items costs about `size · (1 + ln(N / size))` random draws,
/// not N — and the sample is as uniform as one random draw per item (the
/// classic Algorithm R) makes it.  The draws come from SplitMix64 (the
/// kernels' [`mix`](crate::kernels::mix) over a counter) seeded once from
/// the database's random numbers (or a `seed`), not from the operating
/// system per item.  `ANALYZE` ([`crate::stats::Sample`]) and
/// `reservoir_sample` draw by it.
pub(crate) struct Reservoir {
    size: usize,
    /// SplitMix64's counter.
    state: u64,
    /// The largest of `size` uniforms so far, in Algorithm L's terms `W`.
    w: f64,
    /// The (1-based) position of the next item the reservoir takes.
    next: u64,
}

impl Reservoir {
    pub(crate) fn new(size: usize, seed: u64) -> Self {
        let mut r = Self {
            size,
            state: seed,
            w: 1.0,
            next: size as u64,
        };
        r.w = r.shrink(1.0);
        r.next = r.after(size as u64);
        r
    }

    /// A reservoir merged from others ([`Sample::merge`]): it takes no
    /// more items, its generator left for shuffles and draws.
    fn merged(size: usize, state: u64) -> Self {
        Self {
            size,
            state,
            w: 0.0,
            next: u64::MAX,
        }
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    fn next_u64(&mut self) -> u64 {
        splitmix(&mut self.state)
    }

    /// Lemire's unbiased draw of `0..bound` (`bound > 0`).
    fn below(&mut self, bound: u64) -> u64 {
        loop {
            let m = u128::from(self.next_u64()) * u128::from(bound);
            let low = m as u64;
            if low >= bound.wrapping_neg() % bound {
                return (m >> 64) as u64;
            }
        }
    }

    /// A uniform in (0, 1), never 0.
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// `w` times the largest of `size` new uniforms.
    fn shrink(&mut self, w: f64) -> f64 {
        w * (self.uniform().ln() / self.size as f64).exp()
    }

    /// The position taken after `at`: a geometric gap.
    fn after(&mut self, at: u64) -> u64 {
        let gap = (self.uniform().ln() / (1.0 - self.w).ln()).floor();
        // A gap past what a u64 holds (w underflowed): never again.
        let gap = if gap.is_finite() && gap < u64::MAX as f64 {
            gap as u64
        } else {
            u64::MAX
        };
        at.saturating_add(gap).saturating_add(1)
    }

    /// Whether the `seen`-th item (1-based, past the first `size`, each
    /// offered in turn) replaces one in the reservoir, and which.
    pub(crate) fn offer(&mut self, seen: u64) -> Option<usize> {
        if seen != self.next {
            return None;
        }
        let slot = (self.next_u64() % self.size as u64) as usize;
        self.w = self.shrink(self.w);
        self.next = self.after(seen);
        Some(slot)
    }
}

/// A group's reservoir.
struct Sample {
    n: usize,
    seen: u64,
    values: Vec<Value>,
    reservoir: Reservoir,
}

impl Sample {
    fn new(n: usize, seed: u64) -> Sample {
        Sample {
            n,
            seen: 0,
            values: Vec::new(),
            reservoir: Reservoir::new(n, seed),
        }
    }

    fn add(&mut self, v: Value) {
        self.seen += 1;
        if self.values.len() < self.n {
            self.values.push(v);
        } else if let Some(slot) = self.reservoir.offer(self.seen) {
            self.values[slot] = v;
        }
    }

    fn shuffle(&mut self) {
        for i in (1..self.values.len()).rev() {
            let j = self.reservoir.below(i as u64 + 1) as usize;
            self.values.swap(i, j);
        }
    }

    /// Both reservoirs' rows as one: how many values come from each drawn
    /// without replacement, then that many of each's shuffled values.
    fn merge(&mut self, mut other: Sample) {
        if other.seen == 0 {
            return;
        }
        if self.seen == 0 {
            other.n = self.n;
            *self = other;
            return;
        }
        self.shuffle();
        other.shuffle();
        let take = self.n.min((self.seen + other.seen) as usize);
        let (mut na, mut nb) = (self.seen, other.seen);
        let (mut a, mut b) = (self.values.drain(..), other.values.drain(..));
        let mut out = Vec::with_capacity(take);
        let state = self.reservoir.state ^ other.reservoir.state.rotate_left(17);
        let mut rng = Reservoir::merged(self.n, state);
        for _ in 0..take {
            let from_a = rng.below(na + nb) < na;
            let next = if from_a {
                na -= 1;
                a.next()
            } else {
                nb -= 1;
                b.next()
            };
            out.extend(next);
        }
        drop((a, b));
        self.seen += other.seen;
        self.values = out;
        self.reservoir = rng;
    }
}

struct ReservoirSample;

impl Aggregate for ReservoirSample {
    fn name(&self) -> &str {
        "reservoir_sample"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=3
    }
    fn order_free(&self) -> bool {
        false
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let x = args[0].ty.unwrap_or(DataType::Text);
        let returns = x
            .array_of()
            .ok_or_else(|| Error::feature(format!("reservoir_sample of values of type {x}")))?;
        for a in &args[1..] {
            if a.ty.is_some_and(|t| !t.is_integer()) {
                return Err(Error::undefined_function(format!(
                    "function reservoir_sample with an argument of type {} does not exist",
                    a.ty.unwrap()
                ))
                .with_hint("n and the seed are integers."));
            }
        }
        let mut a = vec![Arg::Untyped(DataType::Text)];
        a.extend(args[1..].iter().map(|_| Arg::Convert(DataType::BigInt)));
        Ok(Signature::new(a, Returns::Type(Some(returns))))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One(Groups::default()))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups::default()))
    }
}

#[derive(Default)]
struct Groups {
    groups: Vec<Option<Sample>>,
}

fn n_of(v: &Value) -> Result<usize> {
    match v {
        Value::Int(n) if (1..=1_000_000).contains(n) => Ok(*n as usize),
        Value::Int(n) => Err(Error::invalid_parameter(format!(
            "a sample's size must be between 1 and 1000000, not {n}"
        ))),
        Value::Null => Err(Error::null_value("a sample's size must not be NULL")),
        _ => Err(Error::internal("reservoir_sample's n")),
    }
}

impl Groups {
    fn add(&mut self, g: usize, x: Value, rest: impl FnOnce() -> (Value, Value)) -> Result<()> {
        if self.groups[g].is_none() {
            let (n, seed) = rest();
            let seed = match seed {
                Value::Int(s) => s as u64,
                _ => 0,
            };
            self.groups[g] = Some(Sample::new(n_of(&n)?, seed));
        }
        self.groups[g].as_mut().unwrap().add(x);
        Ok(())
    }

    fn result(&self, g: usize) -> Value {
        match &self.groups[g] {
            Some(s) if s.seen > 0 => Value::Array(s.values.clone()),
            _ => Value::Null,
        }
    }
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        if groups > self.groups.len() {
            self.groups.resize_with(groups, || None);
        }
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        for (&r, &g) in rows.iter().zip(groups) {
            let (r, g) = (r as usize, g as usize);
            self.add(g, args[0].value(r), || {
                (
                    args[1].value(r),
                    args.get(2).map_or(Value::Null, |a| a.value(r)),
                )
            })?;
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    /// `[n, seen, rng, values]`.
    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(match &self.groups[group] {
            None => Value::Null,
            Some(s) => Value::Array(vec![
                Value::Int(s.n as i64),
                Value::Int(s.seen as i64),
                Value::Int(s.reservoir.state as i64),
                Value::Array(s.values.clone()),
            ]),
        })
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(parts) = partial else {
            return Ok(());
        };
        let [
            Value::Int(n),
            Value::Int(seen),
            Value::Int(rng),
            Value::Array(values),
        ] = &parts[..]
        else {
            return Err(Error::internal("reservoir_sample's partial state"));
        };
        let other = Sample {
            n: *n as usize,
            seen: *seen as u64,
            reservoir: Reservoir::merged(*n as usize, *rng as u64),
            values: values.clone(),
        };
        match &mut self.groups[group] {
            Some(s) => s.merge(other),
            None => self.groups[group] = Some(other),
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
        let rest = (args[1].clone(), args.get(2).cloned().unwrap_or(Value::Null));
        self.0.add(0, args[0].clone(), || rest)
    }
    fn finish(&mut self) -> Result<Value> {
        self.0.resize(1);
        Ok(self.0.result(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_are_unbiased_enough() {
        let mut r = Reservoir::merged(1, 7);
        let mut counts = [0u32; 7];
        for _ in 0..70_000 {
            counts[r.below(7) as usize] += 1;
        }
        for c in counts {
            assert!((9_500..10_500).contains(&c), "{counts:?}");
        }
    }

    /// Algorithm L's sample is uniform: over many runs, every stretch of
    /// the stream is sampled as often as its share (a chi-squared test,
    /// seeded, so not flaky), and it takes about `k · ln(N / k)` items
    /// past the first `k`, not one draw per item.
    #[test]
    fn algorithm_l_samples_uniformly_with_few_draws() {
        let (k, n, runs, buckets) = (100usize, 10_000u64, 2_000u64, 100usize);
        let mut hits = vec![0u64; buckets];
        let mut taken = 0u64;
        for run in 0..runs {
            let mut r = Reservoir::new(k, 0x5eed ^ run.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut sample: Vec<u64> = (1..=k as u64).collect();
            for seen in k as u64 + 1..=n {
                if let Some(slot) = r.offer(seen) {
                    sample[slot] = seen;
                    taken += 1;
                }
            }
            for s in sample {
                hits[((s - 1) * buckets as u64 / n) as usize] += 1;
            }
        }
        // Each bucket expects runs · k / buckets.
        let expect = (runs * k as u64 / buckets as u64) as f64;
        let chi2: f64 = hits
            .iter()
            .map(|&h| (h as f64 - expect).powi(2) / expect)
            .sum();
        // 99 degrees of freedom: p = 0.001 at about 148.
        assert!(chi2 < 148.0, "chi-squared {chi2}: {hits:?}");
        let per_run = taken as f64 / runs as f64;
        let ideal = k as f64 * (n as f64 / k as f64).ln();
        assert!(
            (per_run - ideal).abs() < ideal * 0.1,
            "{per_run} taken a run, ideal {ideal}"
        );
    }

    /// How often each of 0..20 is in a 5-sample, over many seeds, for a
    /// reservoir fed in one piece and one merged from pieces.
    fn inclusion(pieces: &[std::ops::Range<i64>]) -> Vec<u32> {
        let mut counts = vec![0u32; 20];
        for seed in 0..20_000u64 {
            let mut whole: Option<Sample> = None;
            for (k, p) in pieces.iter().enumerate() {
                let mut s = Sample::new(5, seed.wrapping_mul(31).wrapping_add(k as u64));
                p.clone().for_each(|i| s.add(Value::Int(i)));
                match &mut whole {
                    None => whole = Some(s),
                    Some(w) => w.merge(s),
                }
            }
            let w = whole.unwrap();
            assert_eq!(w.values.len(), 5);
            for v in &w.values {
                let Value::Int(i) = v else { panic!() };
                counts[*i as usize] += 1;
            }
        }
        counts
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // One piece: no merge.
    fn every_value_is_as_likely_merged_or_not() {
        // Each of 20 values in a 5-sample: a quarter of 20,000 times.
        for pieces in [vec![0..20], vec![0..3, 3..17, 17..20], vec![0..12, 12..20]] {
            for (i, c) in inclusion(&pieces).into_iter().enumerate() {
                assert!((4_700..5_300).contains(&c), "{pieces:?}: {i} {c}");
            }
        }
    }

    #[test]
    fn hash_unit_is_stable() {
        // Pinned: a change of the hash or the encoding is a format change.
        let h = unit(xxh64(&key_bytes(&[Value::Int(42)]), 0));
        assert_eq!(h, unit(crate::stablehash::hash_int(42)));
        assert!((0.0..1.0).contains(&h));
        assert_eq!(seed_bits(-0.0), 0);
    }
}
