//! Drift and tests (PLAN.md §M14.27 S8): two samples of one column told
//! apart by a `side` — `false` the reference, `true` the one compared —
//! and how far apart they are.
//!
//! **Over the values in order** (`ks_test`, `auc`, `psi`): each side's
//! values are kept as the ordered sets keep theirs
//! ([`crate::quantile::Doubles`]: 8-byte keys, spilled past `work_mem`,
//! combined from the workers), then walked once in ascending order, both
//! sides merged — the larger side's distinct values read back from
//! scratch space when they outgrow memory.
//!
//! - `ks_test(x, side)` — the two-sample Kolmogorov–Smirnov test, as
//!   scipy's `ks_2samp`: `{statistic, p-value}`.  The statistic, the
//!   largest gap between the two empirical distribution functions, is
//!   found in integers (`|a·n − b·m|` over `m·n`), so it is exact.  The
//!   p-value is exact (the probability that a random interleaving of the
//!   two samples leaves the band, by the lattice-path recurrence) while
//!   neither side has more than 10,000 values, as scipy's default; past
//!   that, the Kolmogorov distribution with Stephens' correction.
//! - `auc(score, label)` — the area under the ROC curve, `label` true for
//!   the positives: the Mann–Whitney rank sum, ties counted half, in
//!   integers, divided once.
//! - `psi(x, side [, bins])` — the population stability index over
//!   `bins` (10) bins of equal frequency in the reference, ties kept in
//!   one bin: `Σ (q − p)·ln(q / p)`, an empty bin's share taken as 10⁻⁴
//!   where the other side's is not empty.
//!
//! **Over the values' counts** (`js_divergence`, `chisq_test`): a group
//! counts each distinct value (by its key encoding, any type) on each
//! side, at most `work_mem` of them; the sums run over the values in
//! their encoding's order, so the result does not depend on the rows'.
//!
//! - `js_divergence(x, side)` — the Jensen–Shannon divergence of the two
//!   sides' distributions, base 2 (0 to 1; scipy's
//!   `jensenshannon(p, q, base=2) ** 2`).
//! - `chisq_test(x, side)` — Pearson's χ² test of homogeneity on the
//!   2 × k table, as scipy's `chi2_contingency` (Yates' correction at one
//!   degree of freedom): `{statistic, p-value, degrees of freedom}`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, GroupAccumulator, Registry, Returns, Signature,
};
use crate::quantile::Doubles;
use crate::spill::Run;
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// Registers the drift and test aggregates.
pub fn register(r: &mut Registry) -> Result<()> {
    for (test, name) in [
        (Ordered::Ks, "ks_test"),
        (Ordered::Auc, "auc"),
        (Ordered::Psi, "psi"),
    ] {
        r.add_aggregate(&[name], Arc::new(OrderedTest(test, name)))?;
    }
    for (test, name) in [
        (Counted::Js, "js_divergence"),
        (Counted::ChiSq, "chisq_test"),
    ] {
        r.add_aggregate(&[name], Arc::new(CountedTest(test, name)))?;
    }
    Ok(())
}

fn float_array() -> DataType {
    DataType::Float.array_of().expect("double precision[]")
}

/// The `side` argument: a boolean (an integer `0` / `1` converts).
fn side_of(name: &str, args: &[ArgInfo]) -> Result<()> {
    match args[1].ty {
        None | Some(DataType::Bool) => Ok(()),
        Some(t) if t.is_integer() => Ok(()),
        Some(t) => Err(Error::undefined_function(format!(
            "function {name} with a side of type {t} does not exist"
        ))
        .with_hint("The side is a boolean: false the reference, true the sample compared.")),
    }
}

// ------------------------------------------------------ over the values

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ordered {
    Ks,
    Auc,
    Psi,
}

struct OrderedTest(Ordered, &'static str);

impl Aggregate for OrderedTest {
    fn name(&self) -> &str {
        self.1
    }
    fn arity(&self) -> RangeInclusive<usize> {
        if self.0 == Ordered::Psi { 2..=3 } else { 2..=2 }
    }
    fn order_free(&self) -> bool {
        true
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if let Some(t) = args[0].ty
            && !(t.is_integer()
                || matches!(t, DataType::Float | DataType::Real | DataType::Numeric))
        {
            return Err(Error::undefined_function(format!(
                "function {}({t}, …) does not exist",
                self.1
            ))
            .with_hint("Its values are numbers."));
        }
        side_of(self.1, args)?;
        let mut a = vec![Arg::Convert(DataType::Float), Arg::Convert(DataType::Bool)];
        if args.len() == 3 {
            if args[2].ty.is_some_and(|t| !t.is_integer()) {
                return Err(Error::undefined_function(
                    "function psi with bins not an integer does not exist",
                ));
            }
            a.push(Arg::Convert(DataType::BigInt));
        }
        let returns = match self.0 {
            Ordered::Ks => float_array(),
            Ordered::Auc | Ordered::Psi => DataType::Float,
        };
        Ok(Signature::new(a, Returns::Type(Some(returns))))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One::Samples(Box::new(Samples::new(self.0))))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Samples::new(self.0)))
    }
}

/// The groups of a test over values in order: group `g`'s reference in
/// the store's group `2g`, the sample compared in `2g + 1`.
struct Samples {
    test: Ordered,
    store: Doubles,
    /// `psi`'s bins (from a group's first row).
    bins: Vec<Option<i64>>,
}

impl Samples {
    fn new(test: Ordered) -> Samples {
        Samples {
            test,
            store: Doubles::new(),
            bins: Vec::new(),
        }
    }

    fn add(&mut self, g: usize, x: &Value, side: &Value, bins: Option<Value>) -> Result<()> {
        let (Value::Float(x), Value::Bool(side)) = (x, side) else {
            return Ok(()); // a NULL value or side
        };
        if self.bins[g].is_none()
            && let Some(Value::Int(b)) = bins
        {
            if !(1..=10_000).contains(&b) {
                return Err(Error::invalid_parameter(format!(
                    "psi's bins must be between 1 and 10000, not {b}"
                )));
            }
            self.bins[g] = Some(b);
        }
        self.store.push(2 * g + usize::from(*side), *x)
    }

    fn result(&mut self, g: usize) -> Result<Value> {
        let (m, n) = (self.store.count(2 * g), self.store.count(2 * g + 1));
        if m == 0 || n == 0 {
            return Ok(Value::Null);
        }
        Ok(match self.test {
            Ordered::Ks => {
                let mut ks = Ks::new(m, n);
                walk(&mut self.store, g, &mut |_, a, b| ks.step(a, b))?;
                let (d, p) = ks.finish();
                Value::Array(vec![Value::Float(d), Value::Float(p)])
            }
            Ordered::Auc => {
                let mut auc = Auc::default();
                walk(&mut self.store, g, &mut |_, a, b| auc.step(a, b))?;
                Value::Float(auc.finish(m, n))
            }
            Ordered::Psi => {
                let mut psi = Psi::new(m, self.bins[g].unwrap_or(10) as usize);
                walk(&mut self.store, g, &mut |_, a, b| psi.step(a, b))?;
                Value::Float(psi.finish(m, n))
            }
        })
    }
}

impl GroupAccumulator for Samples {
    fn resize(&mut self, groups: usize) {
        if groups > self.bins.len() {
            self.bins.resize(groups, None);
            self.store.resize(2 * groups);
        }
    }
    fn group_bytes(&self) -> usize {
        128
    }
    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        for (&r, &g) in rows.iter().zip(groups) {
            let r = r as usize;
            let bins = args.get(2).map(|a| a.value(r));
            self.add(g as usize, &args[0].value(r), &args[1].value(r), bins)?;
        }
        Ok(())
    }
    fn combinable(&self) -> bool {
        true
    }
    /// `[reference, compared, bins]`, the first two the store's partials.
    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(Value::Array(vec![
            self.store.partial(2 * group)?,
            self.store.partial(2 * group + 1)?,
            self.bins[group].map_or(Value::Null, Value::Int),
        ]))
    }
    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(parts) = partial else {
            return Err(Error::internal("a two-sample test's partial state"));
        };
        let [a, b, bins] = <[Value; 3]>::try_from(parts)
            .map_err(|_| Error::internal("a two-sample test's partial state"))?;
        self.store.combine(2 * group, a)?;
        self.store.combine(2 * group + 1, b)?;
        if let Value::Int(b) = bins {
            self.bins[group].get_or_insert(b);
        }
        Ok(())
    }
    fn finish(&mut self, group: usize) -> Result<Value> {
        self.result(group)
    }
}

/// The distinct values of one side, as a walk reads them back: in memory
/// up to half of `work_mem`, then in scratch space.
enum Runs {
    Memory(Vec<(f64, u64)>),
    Scratch(Run),
}

impl Runs {
    fn push(&mut self, x: f64, n: u64, limit: usize) -> Result<()> {
        if let Runs::Memory(v) = self {
            if v.len() * 16 < limit {
                v.push((x, n));
                return Ok(());
            }
            let mut run = Run::new()?;
            for (x, n) in v.drain(..) {
                run.push_record(&[&x.to_le_bytes(), &n.to_le_bytes()])?;
            }
            *self = Runs::Scratch(run);
        }
        let Runs::Scratch(run) = self else {
            unreachable!()
        };
        run.push_record(&[&x.to_le_bytes(), &n.to_le_bytes()])
    }
}

/// A reader of [`Runs`].
enum Reader {
    Memory(std::vec::IntoIter<(f64, u64)>),
    Scratch(crate::spill::RunIter),
}

impl Reader {
    fn of(runs: Runs) -> Result<Reader> {
        Ok(match runs {
            Runs::Memory(v) => Reader::Memory(v.into_iter()),
            Runs::Scratch(run) => Reader::Scratch(run.seal()?.iter()),
        })
    }

    fn next(&mut self) -> Result<Option<(f64, u64)>> {
        match self {
            Reader::Memory(it) => Ok(it.next()),
            Reader::Scratch(it) => {
                let Some(r) = it.next_record()? else {
                    return Ok(None);
                };
                let (x, n) = r
                    .split_first_chunk::<8>()
                    .and_then(|(x, rest)| Some((*x, *rest.first_chunk::<8>()?)))
                    .ok_or_else(|| Error::internal("a two-sample walk's record"))?;
                Ok(Some((f64::from_le_bytes(x), u64::from_le_bytes(n))))
            }
        }
    }
}

/// Walks group `g`'s distinct values of both sides in ascending order:
/// `step(x, a, b)` with how many of the reference (`a`) and of the sample
/// compared (`b`) equal `x`.  The groups' values are gone afterwards.
fn walk(store: &mut Doubles, g: usize, step: &mut dyn FnMut(f64, u64, u64)) -> Result<()> {
    let limit = crate::budget::work_mem() / 2;
    let mut runs = Runs::Memory(Vec::new());
    let mut failed = None;
    store.visit(2 * g + 1, &mut |x, n| {
        if failed.is_none()
            && let Err(e) = runs.push(x, n, limit)
        {
            failed = Some(e);
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    let mut compared = Reader::of(runs)?;
    let mut next = compared.next()?;
    store.visit(2 * g, &mut |x, a| {
        if failed.is_some() {
            return;
        }
        let mut b = 0;
        while let Some((y, n)) = next {
            match Doubles::cmp(y, x) {
                Ordering::Less => step(y, 0, n),
                Ordering::Equal => b = n,
                Ordering::Greater => break,
            }
            next = match compared.next() {
                Ok(n) => n,
                Err(e) => {
                    failed = Some(e);
                    return;
                }
            };
        }
        step(x, a, b);
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    while let Some((y, n)) = next {
        step(y, 0, n);
        next = compared.next()?;
    }
    Ok(())
}

/// The Kolmogorov–Smirnov statistic as a walk finds it.
struct Ks {
    m: u64,
    n: u64,
    a: u64,
    b: u64,
    /// The largest `|a·n − b·m|`.
    h: u128,
}

impl Ks {
    fn new(m: u64, n: u64) -> Ks {
        Ks {
            m,
            n,
            a: 0,
            b: 0,
            h: 0,
        }
    }

    fn step(&mut self, a: u64, b: u64) {
        self.a += a;
        self.b += b;
        let (x, y) = (
            u128::from(self.a) * u128::from(self.n),
            u128::from(self.b) * u128::from(self.m),
        );
        self.h = self.h.max(x.abs_diff(y));
    }

    /// The statistic and its two-sided p-value.
    fn finish(&self) -> (f64, f64) {
        let (m, n) = (self.m, self.n);
        let d = self.h as f64 / (m as f64 * n as f64);
        let p = if self.h == 0 {
            1.0
        } else if m.max(n) <= 10_000 {
            ks_exact(m, n, self.h)
        } else {
            let en = (m as f64 * n as f64) / (m as f64 + n as f64);
            let s = en.sqrt();
            kolmogorov((s + 0.12 + 0.11 / s) * d)
        };
        (d, p.clamp(0.0, 1.0))
    }
}

/// The probability that a random interleaving of `m` and `n` values
/// reaches `|i·n − j·m| ≥ h` somewhere: the share of the lattice paths
/// from `(0, 0)` to `(m, n)` that leave the band, each point's share from
/// its two predecessors' (`i / (i + j)` and `j / (i + j)` of the paths
/// come from each) — the outside counted directly, so a small p-value
/// keeps its digits.
fn ks_exact(m: u64, n: u64, h: u128) -> f64 {
    let out = |i: u64, j: u64| {
        (u128::from(i) * u128::from(n)).abs_diff(u128::from(j) * u128::from(m)) >= h
    };
    // q[j]: the share of paths to (i, j) that have left the band.
    let mut q = vec![0.0f64; n as usize + 1];
    for j in 1..=n {
        q[j as usize] = if out(0, j) { 1.0 } else { q[j as usize - 1] };
    }
    for i in 1..=m {
        q[0] = if out(i, 0) { 1.0 } else { q[0] };
        for j in 1..=n {
            let ju = j as usize;
            q[ju] = if out(i, j) {
                1.0
            } else {
                let s = (i + j) as f64;
                q[ju] * (i as f64 / s) + q[ju - 1] * (j as f64 / s)
            };
        }
    }
    q[n as usize]
}

/// The Kolmogorov distribution's upper tail `P(K > λ)`: its alternating
/// series for `λ ≥ 1.18`, else one minus the Jacobi-theta form of the
/// lower tail (each converging in a few terms where the other is slow).
fn kolmogorov(lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return 1.0;
    }
    if lambda < 1.18 {
        let pi = std::f64::consts::PI;
        let w = -(pi * pi) / (8.0 * lambda * lambda);
        let mut s = 0.0;
        for k in [1.0f64, 3.0, 5.0, 7.0, 9.0, 11.0] {
            s += (k * k * w).exp();
        }
        1.0 - (2.0 * pi).sqrt() / lambda * s
    } else {
        let mut s = 0.0;
        let mut sign = 1.0;
        for k in 1..=20 {
            let k = f64::from(k);
            let t = (-2.0 * k * k * lambda * lambda).exp();
            s += sign * t;
            if t < 1e-17 {
                break;
            }
            sign = -sign;
        }
        2.0 * s
    }
}

/// The rank sum: positives (the side `true`) above negatives, ties half.
#[derive(Default)]
struct Auc {
    below: u128,
    /// Twice the Mann–Whitney U.
    u2: u128,
}

impl Auc {
    fn step(&mut self, neg: u64, pos: u64) {
        self.u2 += u128::from(pos) * (2 * self.below + u128::from(neg));
        self.below += u128::from(neg);
    }

    fn finish(&self, m: u64, n: u64) -> f64 {
        self.u2 as f64 / (2.0 * m as f64 * n as f64)
    }
}

/// The population stability index's bins as a walk fills them.
struct Psi {
    m: u64,
    bins: usize,
    /// The reference's values so far, and the bin a value falls in.
    a: u64,
    bin: usize,
    counts: Vec<[u64; 2]>,
}

impl Psi {
    fn new(m: u64, bins: usize) -> Psi {
        Psi {
            m,
            bins,
            a: 0,
            bin: 0,
            counts: vec![[0; 2]; bins],
        }
    }

    /// The reference's rank the `k`-th bin starts at (`k` in `1..bins`).
    fn edge(&self, k: usize) -> u64 {
        (u128::from(self.m) * k as u128 / self.bins as u128) as u64
    }

    fn step(&mut self, a: u64, b: u64) {
        // The bin of x: the edges (the reference's values at the ranks
        // `edge(k)`) at or below x — those whose rank is below the
        // reference's count up to x.
        self.a += a;
        while self.bin + 1 < self.bins && self.edge(self.bin + 1) < self.a {
            self.bin += 1;
        }
        self.counts[self.bin][0] += a;
        self.counts[self.bin][1] += b;
    }

    fn finish(&self, m: u64, n: u64) -> f64 {
        let mut psi = 0.0;
        for [a, b] in &self.counts {
            if *a == 0 && *b == 0 {
                continue;
            }
            let p = (*a as f64 / m as f64).max(1e-4);
            let q = (*b as f64 / n as f64).max(1e-4);
            psi += (q - p) * (q / p).ln();
        }
        psi
    }
}

// ------------------------------------------------------ over the counts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counted {
    Js,
    ChiSq,
}

struct CountedTest(Counted, &'static str);

impl Aggregate for CountedTest {
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
        side_of(self.1, args)?;
        let returns = match self.0 {
            Counted::Js => DataType::Float,
            Counted::ChiSq => float_array(),
        };
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Text), Arg::Convert(DataType::Bool)],
            Returns::Type(Some(returns)),
        ))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One::Table(Table::new(self.0, self.1)))
    }
    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Table::new(self.0, self.1)))
    }
}

/// Each group's counts of each distinct value on each side, keyed by the
/// value's key encoding.
struct Table {
    test: Counted,
    name: &'static str,
    groups: Vec<HashMap<Vec<u8>, [u64; 2]>>,
    held: usize,
    limit: usize,
}

impl Table {
    fn new(test: Counted, name: &'static str) -> Table {
        Table {
            test,
            name,
            groups: Vec::new(),
            held: 0,
            limit: crate::budget::work_mem(),
        }
    }

    fn count(&mut self, g: usize, key: Vec<u8>, side: usize, n: u64) -> Result<()> {
        let len = key.len();
        let counts = self.groups[g].entry(key).or_insert_with(|| {
            self.held += len + 48;
            [0; 2]
        });
        counts[side] += n;
        if self.held > self.limit {
            return Err(Error::program_limit(format!(
                "{} counts more distinct values than work_mem holds",
                self.name
            ))
            .with_hint("Bin the values first (width_bucket), or raise work_mem."));
        }
        Ok(())
    }

    fn add(&mut self, g: usize, x: &Value, side: &Value) -> Result<()> {
        let Value::Bool(side) = side else {
            return Ok(());
        };
        if x.is_null() {
            return Ok(());
        }
        let mut key = Vec::new();
        crate::encoding::encode_key_value(&mut key, x);
        self.count(g, key, usize::from(*side), 1)
    }

    /// The group's counts in their values' order.
    fn sorted(&self, g: usize) -> Vec<[u64; 2]> {
        let mut rows: Vec<(&Vec<u8>, &[u64; 2])> = self.groups[g].iter().collect();
        rows.sort_unstable_by(|x, y| x.0.cmp(y.0));
        rows.into_iter().map(|(_, c)| *c).collect()
    }

    fn result(&mut self, g: usize) -> Result<Value> {
        let rows = self.sorted(g);
        let m: u64 = rows.iter().map(|c| c[0]).sum();
        let n: u64 = rows.iter().map(|c| c[1]).sum();
        if m == 0 || n == 0 {
            return Ok(Value::Null);
        }
        Ok(match self.test {
            Counted::Js => Value::Float(js(&rows, m, n)),
            Counted::ChiSq => {
                let (stat, p, dof) = chisq(&rows, m, n);
                Value::Array(vec![
                    Value::Float(stat),
                    Value::Float(p),
                    Value::Float(dof as f64),
                ])
            }
        })
    }
}

/// The Jensen–Shannon divergence, base 2.
fn js(rows: &[[u64; 2]], m: u64, n: u64) -> f64 {
    let mut s = 0.0;
    for [a, b] in rows {
        let p = *a as f64 / m as f64;
        let q = *b as f64 / n as f64;
        let mid = (p + q) / 2.0;
        if p > 0.0 {
            s += p * (p / mid).log2();
        }
        if q > 0.0 {
            s += q * (q / mid).log2();
        }
    }
    (s / 2.0).max(0.0)
}

/// Pearson's χ² on the 2 × k table (k the values seen), Yates' correction
/// at one degree of freedom (as scipy's `chi2_contingency`): the
/// statistic, its p-value, the degrees of freedom.
fn chisq(rows: &[[u64; 2]], m: u64, n: u64) -> (f64, f64, u64) {
    let dof = rows.len() as u64 - 1;
    if dof == 0 {
        return (0.0, 1.0, 0);
    }
    let total = (m + n) as f64;
    let mut stat = 0.0;
    for c in rows {
        let col = (c[0] + c[1]) as f64;
        for (side, size) in [(0, m), (1, n)] {
            let expected = size as f64 * col / total;
            let observed = c[side] as f64;
            let mut diff = observed - expected;
            if dof == 1 {
                diff = diff.signum() * (diff.abs() - 0.5).max(0.0);
            }
            stat += diff * diff / expected;
        }
    }
    (stat, gamma_q(dof as f64 / 2.0, stat / 2.0), dof)
}

/// `ln Γ(x)` for `x > 0` (Lanczos, g = 7, nine terms: about 15 digits).
fn ln_gamma(x: f64) -> f64 {
    const G: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        let pi = std::f64::consts::PI;
        return (pi / (pi * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = G[0];
    let t = x + 7.5;
    for (i, g) in G.iter().enumerate().skip(1) {
        a += g / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// The regularized upper incomplete gamma function `Q(a, x)`: χ²'s upper
/// tail at `2x` with `2a` degrees of freedom — its series below `a + 1`,
/// its continued fraction (Lentz) above.
fn gamma_q(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    let front = (-x + a * x.ln() - ln_gamma(a)).exp();
    if x < a + 1.0 {
        let (mut sum, mut term, mut ap) = (1.0 / a, 1.0 / a, a);
        for _ in 0..1000 {
            ap += 1.0;
            term *= x / ap;
            sum += term;
            if term.abs() < sum.abs() * 1e-16 {
                break;
            }
        }
        (1.0 - sum * front).max(0.0)
    } else {
        let tiny = 1e-300;
        let mut b = x + 1.0 - a;
        let mut c = 1.0 / tiny;
        let mut d = 1.0 / b;
        let mut h = d;
        for i in 1..1000 {
            let an = -f64::from(i) * (f64::from(i) - a);
            b += 2.0;
            d = an * d + b;
            if d.abs() < tiny {
                d = tiny;
            }
            c = b + an / c;
            if c.abs() < tiny {
                c = tiny;
            }
            d = 1.0 / d;
            let delta = d * c;
            h *= delta;
            if (delta - 1.0).abs() < 1e-16 {
                break;
            }
        }
        (front * h).min(1.0)
    }
}

impl GroupAccumulator for Table {
    fn resize(&mut self, groups: usize) {
        if groups > self.groups.len() {
            self.groups.resize_with(groups, HashMap::new);
        }
    }
    fn group_bytes(&self) -> usize {
        64
    }
    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        for (&r, &g) in rows.iter().zip(groups) {
            let r = r as usize;
            self.add(g as usize, &args[0].value(r), &args[1].value(r))?;
        }
        Ok(())
    }
    fn combinable(&self) -> bool {
        true
    }
    /// `[key, reference count, compared count, …]`.
    fn partial(&mut self, group: usize) -> Result<Value> {
        let mut out = Vec::with_capacity(3 * self.groups[group].len());
        for (k, [a, b]) in &self.groups[group] {
            out.push(Value::Bytea(k.clone().into()));
            out.push(Value::Int(*a as i64));
            out.push(Value::Int(*b as i64));
        }
        Ok(Value::Array(out))
    }
    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(parts) = partial else {
            return Err(Error::internal("a contingency table's partial state"));
        };
        for t in parts.chunks(3) {
            let [Value::Bytea(k), Value::Int(a), Value::Int(b)] = t else {
                return Err(Error::internal("a contingency table's partial state"));
            };
            self.count(group, k.to_vec(), 0, *a as u64)?;
            self.count(group, k.to_vec(), 1, *b as u64)?;
        }
        Ok(())
    }
    fn finish(&mut self, group: usize) -> Result<Value> {
        self.result(group)
    }
}

/// One group's state (the row-at-a-time executor's).
enum One {
    Samples(Box<Samples>),
    Table(Table),
}

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        match self {
            One::Samples(s) => {
                s.resize(1);
                s.add(0, &args[0], &args[1], args.get(2).cloned())
            }
            One::Table(t) => {
                t.resize(1);
                t.add(0, &args[0], &args[1])
            }
        }
    }
    fn finish(&mut self) -> Result<Value> {
        match self {
            One::Samples(s) => {
                s.resize(1);
                s.result(0)
            }
            One::Table(t) => {
                t.resize(1);
                t.result(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_ks_p_values_are_scipys() {
        // scipy.stats.ks_2samp(…, method='exact'): small cases by hand.
        // m = n = 3, D = 1 (h = 9): 2 / C(6, 3) = 0.1.
        assert!((ks_exact(3, 3, 9) - 0.1).abs() < 1e-15);
        // m = n = 1: D = 1 always.
        assert_eq!(ks_exact(1, 1, 1), 1.0);
    }

    #[test]
    fn the_gamma_tail_is_chi_squared() {
        // χ² with 1 degree of freedom at 3.841458820694124: p = 0.05.
        assert!((gamma_q(0.5, 3.841_458_820_694_124 / 2.0) - 0.05).abs() < 1e-12);
        // 10 degrees of freedom at 18.307038053275146: 0.05.
        assert!((gamma_q(5.0, 18.307_038_053_275_146 / 2.0) - 0.05).abs() < 1e-12);
        assert!((ln_gamma(10.0) - 362_880f64.ln()).abs() < 1e-12);
    }

    #[test]
    fn the_kolmogorov_tail_is_continuous() {
        let (a, b) = (kolmogorov(1.18 - 1e-9), kolmogorov(1.18 + 1e-9));
        assert!((a - b).abs() < 1e-8, "{a} {b}");
        // P(K > 1.358099) = 0.05.
        assert!((kolmogorov(1.358_099) - 0.05).abs() < 1e-5);
    }
}
