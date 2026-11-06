//! The most frequent values (PLAN.md §M14.27 S6): `approx_top_k(x, k)`
//! and `approx_top_k_counts(x, k)`, by Misra and Gries's frequent-items
//! summary ("Finding repeated elements", 1982) in the batched, mergeable
//! form of Agarwal et al. ("Mergeable Summaries", PODS 2012).
//!
//! A group counts its values exactly — by their key encoding, so equal
//! values are one — until it holds more than `2C` of them (`C =
//! max(1024, 8k)`); then the `(C + 1)`-th greatest count is taken from
//! every count and the values left at zero are dropped.  Two summaries
//! merge by adding counts and reducing the same way.  A count is then
//! low by at most `(n − Σ counts) / (C + 1)`, so every value more
//! frequent than `n / (C + 1)` is kept; while `x` has at most `2C`
//! distinct values nothing is dropped and the result is exact.
//!
//! The result: the `k` values with the greatest counts, ties to the least
//! value — a function of the counts.  Past `2C` distinct values the counts
//! depend on where the reductions fell, so the aggregate is not
//! order-free: its morsels' summaries are merged in the morsels' order,
//! the same in every run of a plan (a sketch whose merge is the same in
//! any order — as `hll`'s and `ddsketch`'s are — does not exist for
//! frequent items whose values must be returned).

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, GroupAccumulator, Registry, Returns, Signature,
};
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// The most values `k` may ask for.
const MAX_K: i64 = 10_000;

/// A group's summary.
#[derive(Clone, Debug, Default)]
struct Summary {
    k: usize,
    /// Rows counted.
    n: u64,
    /// By key encoding: the value and its count.
    counts: HashMap<Box<[u8]>, (Value, u64)>,
}

impl Summary {
    fn new(k: usize) -> Summary {
        Summary {
            k,
            ..Summary::default()
        }
    }

    fn capacity(&self) -> usize {
        (8 * self.k).max(1024)
    }

    fn add(&mut self, v: Value, n: u64) {
        let mut key = Vec::with_capacity(16);
        crate::encoding::encode_key_value(&mut key, &v);
        self.add_keyed(key.into(), v, n);
    }

    fn add_keyed(&mut self, key: Box<[u8]>, v: Value, n: u64) {
        self.counts.entry(key).or_insert((v, 0)).1 += n;
        if self.counts.len() > 2 * self.capacity() {
            self.reduce();
        }
    }

    /// Takes the `(C + 1)`-th greatest count from every count.
    fn reduce(&mut self) {
        let c = self.capacity();
        if self.counts.len() <= c {
            return;
        }
        let mut all: Vec<u64> = self.counts.values().map(|e| e.1).collect();
        let (_, &mut t, _) = all.select_nth_unstable_by(c, |a, b| b.cmp(a));
        self.counts.retain(|_, e| {
            e.1 = e.1.saturating_sub(t);
            e.1 > 0
        });
    }

    fn merge(&mut self, other: Summary) {
        self.n += other.n;
        for (key, (v, n)) in other.counts {
            self.add_keyed(key, v, n);
        }
    }

    /// The `k` greatest counts, ties to the least value.
    fn top(&self) -> Vec<(&Value, u64)> {
        let mut all: Vec<(&[u8], &Value, u64)> = self
            .counts
            .iter()
            .map(|(key, (v, n))| (&key[..], v, *n))
            .collect();
        all.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        all.into_iter()
            .take(self.k)
            .map(|(_, v, n)| (v, n))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Values,
    Counts,
}

struct TopK(Kind, &'static str);

/// Registers `approx_top_k` and `approx_top_k_counts`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_aggregate(
        &["approx_top_k"],
        Arc::new(TopK(Kind::Values, "approx_top_k")),
    )?;
    r.add_aggregate(
        &["approx_top_k_counts"],
        Arc::new(TopK(Kind::Counts, "approx_top_k_counts")),
    )?;
    Ok(())
}

impl Aggregate for TopK {
    fn name(&self) -> &str {
        self.1
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn order_free(&self) -> bool {
        false
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let x = args[0].ty.unwrap_or(DataType::Text);
        let returns = match self.0 {
            Kind::Values => x
                .array_of()
                .ok_or_else(|| Error::feature(format!("{} of values of type {x}", self.1)))?,
            Kind::Counts => DataType::Array(&DataType::BigInt),
        };
        if args[1].ty.is_some_and(|t| !t.is_integer()) {
            return Err(Error::undefined_function(format!(
                "function {}({x}, {}) does not exist",
                self.1,
                args[1].ty.unwrap()
            ))
            .with_hint("k is how many values: an integer."));
        }
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Text), Arg::Convert(DataType::BigInt)],
            Returns::Type(Some(returns)),
        ))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One(Groups::new(self.0)))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups::new(self.0)))
    }
}

fn k_of(v: &Value) -> Result<usize> {
    match v {
        Value::Int(k) if (1..=MAX_K).contains(k) => Ok(*k as usize),
        Value::Int(k) => Err(Error::invalid_parameter(format!(
            "k must be between 1 and {MAX_K}, not {k}"
        ))),
        Value::Null => Err(Error::null_value("k must not be NULL")),
        _ => Err(Error::internal("approx_top_k's k")),
    }
}

struct Groups {
    kind: Kind,
    groups: Vec<Option<Summary>>,
}

impl Groups {
    fn new(kind: Kind) -> Groups {
        Groups {
            kind,
            groups: Vec::new(),
        }
    }

    fn add(&mut self, g: usize, x: Value, k: impl FnOnce() -> Value) -> Result<()> {
        if self.groups[g].is_none() {
            self.groups[g] = Some(Summary::new(k_of(&k())?));
        }
        let s = self.groups[g].as_mut().unwrap();
        s.n += 1;
        s.add(x, 1);
        Ok(())
    }

    fn result(&self, g: usize) -> Value {
        let Some(s) = &self.groups[g] else {
            return Value::Null;
        };
        let top = s.top();
        Value::Array(match self.kind {
            Kind::Values => top.into_iter().map(|(v, _)| v.clone()).collect(),
            Kind::Counts => top.into_iter().map(|(_, n)| Value::Int(n as i64)).collect(),
        })
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
        for (&r, &g) in rows.iter().zip(groups) {
            let (r, g) = (r as usize, g as usize);
            self.add(g, args[0].value(r), || args[1].value(r))?;
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    /// `[k, n, values, counts]`.
    fn partial(&mut self, group: usize) -> Result<Value> {
        let Some(s) = &self.groups[group] else {
            return Ok(Value::Null);
        };
        let (values, counts): (Vec<Value>, Vec<Value>) = s
            .counts
            .values()
            .map(|(v, n)| (v.clone(), Value::Int(*n as i64)))
            .unzip();
        Ok(Value::Array(vec![
            Value::Int(s.k as i64),
            Value::Int(s.n as i64),
            Value::Array(values),
            Value::Array(counts),
        ]))
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(parts) = partial else {
            return Ok(());
        };
        let [
            Value::Int(k),
            Value::Int(n),
            Value::Array(values),
            Value::Array(counts),
        ] = &parts[..]
        else {
            return Err(Error::internal("approx_top_k's partial state"));
        };
        let mut other = Summary::new(*k as usize);
        other.n = *n as u64;
        for (v, c) in values.iter().zip(counts) {
            let Value::Int(c) = c else {
                return Err(Error::internal("approx_top_k's partial state"));
            };
            other.add(v.clone(), *c as u64);
        }
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
        let k = args[1].clone();
        self.0.add(0, args[0].clone(), || k)
    }
    fn finish(&mut self) -> Result<Value> {
        self.0.resize(1);
        Ok(self.0.result(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skewed stream: value `i` about `1000 / (i + 1)` times, then many
    /// values once.
    fn stream() -> Vec<i64> {
        let mut v = Vec::new();
        for i in 0..200i64 {
            for _ in 0..(10_000 / (i + 1)) {
                v.push(i);
            }
        }
        v.extend(1_000_000..1_050_000);
        // Interleaved, as a table would hold them.
        let len = v.len();
        let mut out = vec![0; len];
        for (j, x) in v.into_iter().enumerate() {
            out[(j * 7919) % len] = x;
        }
        out
    }

    fn summary(xs: &[i64], k: usize) -> Summary {
        let mut s = Summary::new(k);
        for &x in xs {
            s.n += 1;
            s.add(Value::Int(x), 1);
        }
        s
    }

    #[test]
    fn frequent_values_are_kept_within_the_bound() {
        let xs = stream();
        let n = xs.len() as u64;
        let mut exact: HashMap<i64, u64> = HashMap::new();
        for &x in &xs {
            *exact.entry(x).or_default() += 1;
        }
        for cuts in [vec![0, xs.len()], vec![0, 1000, 30_000, 77_777, xs.len()]] {
            let mut s = Summary::new(10);
            for w in cuts.windows(2) {
                s.merge(summary(&xs[w[0]..w[1]], 10));
            }
            let kept: u64 = s.counts.values().map(|e| e.1).sum();
            let bound = (n - kept) / (s.capacity() as u64 + 1);
            for (v, est) in s.top() {
                let Value::Int(v) = v else { panic!() };
                let truth = exact[v];
                assert!(
                    est <= truth && truth <= est + bound,
                    "{v}: {est} of {truth}, ±{bound}"
                );
            }
            let top: Vec<_> = s.top().into_iter().map(|(v, _)| v.clone()).collect();
            assert_eq!(top, (0..10).map(Value::Int).collect::<Vec<_>>());
        }
    }

    #[test]
    fn few_distinct_values_are_exact_in_any_order() {
        let xs: Vec<i64> = (0..30_000).map(|i| (i * 31) % 997 % (1 + i % 50)).collect();
        let fwd = summary(&xs, 5);
        let mut back = Summary::new(5);
        for c in xs.chunks(1234).rev() {
            back.merge(summary(c, 5));
        }
        assert_eq!(fwd.top(), back.top());
        let mut exact: HashMap<i64, u64> = HashMap::new();
        for &x in &xs {
            *exact.entry(x).or_default() += 1;
        }
        for (v, n) in fwd.top() {
            let Value::Int(v) = v else { panic!() };
            assert_eq!(exact[v], n);
        }
    }
}
