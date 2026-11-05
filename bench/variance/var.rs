//! Variance methods compared (PLAN.md §M14.27, the study): error, reproducibility
//! across splits among workers, cost a value.  Standalone:
//! `rustc -O -C target-cpu=native --edition 2024 var.rs && ./var`; `np.py` times numpy.
#![allow(dead_code)]
mod exact;
mod superacc;
use exact::ExactSum;
use superacc::SuperAcc;
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-300), self.unit());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

fn naive(xs: &[f64]) -> f64 {
    let (mut s, mut s2) = (0.0, 0.0);
    for &x in xs {
        s += x;
        s2 += x * x;
    }
    let n = xs.len() as f64;
    (s2 - s * s / n) / (n - 1.0)
}

#[derive(Clone, Copy, Default)]
struct Welford {
    n: f64,
    mean: f64,
    m2: f64,
}
impl Welford {
    fn add(&mut self, x: f64) {
        self.n += 1.0;
        let d = x - self.mean;
        self.mean += d / self.n;
        self.m2 += d * (x - self.mean);
    }
    // Chan et al.
    fn merge(&mut self, o: &Welford) {
        if o.n == 0.0 {
            return;
        }
        let n = self.n + o.n;
        let d = o.mean - self.mean;
        self.mean += d * o.n / n;
        self.m2 += o.m2 + d * d * self.n * o.n / n;
        self.n = n;
    }
    fn var(&self) -> f64 {
        self.m2 / (self.n - 1.0)
    }
}

// PostgreSQL's float8_accum (Youngs–Cramer): N, Sx, Sxx.
#[derive(Clone, Copy, Default)]
struct YoungsCramer {
    n: f64,
    sx: f64,
    sxx: f64,
}
impl YoungsCramer {
    fn add(&mut self, x: f64) {
        self.n += 1.0;
        self.sx += x;
        if self.n > 1.0 {
            let t = x * self.n - self.sx;
            self.sxx += t * t / (self.n * (self.n - 1.0));
        }
    }
    fn var(&self) -> f64 {
        self.sxx / (self.n - 1.0)
    }
}

/// Exact power sums: Σx and Σx² kept exactly (x² as hi + lo by FMA).
#[derive(Clone, Default)]
struct Power {
    n: u64,
    s1: ExactSum,
    s2: ExactSum,
}
impl Power {
    fn add(&mut self, x: f64) {
        self.n += 1;
        self.s1.add(x);
        let hi = x * x;
        let lo = x.mul_add(x, -hi);
        self.s2.add(hi);
        if lo != 0.0 {
            self.s2.add(lo);
        }
    }
    fn merge(&mut self, o: &Power) {
        self.n += o.n;
        self.s1.merge(&o.s1);
        self.s2.merge(&o.s2);
    }
    /// Σ(x − m)² = Σx² − 2mΣx + n m², exactly, for m the rounded mean.
    fn var(&self) -> f64 {
        let n = self.n as f64;
        let m = self.s1.total().unwrap() / n;
        let mut acc = ExactSum::new();
        let (p2, _, _) = self.s2.parts();
        for p in p2 {
            acc.add(p);
        }
        let (p1, _, _) = self.s1.parts();
        let two_m = -2.0 * m;
        for p in p1 {
            let hi = two_m * p;
            acc.add(hi);
            acc.add(two_m.mul_add(p, -hi));
        }
        let nm = n * m;
        let nm_lo = n.mul_add(m, -nm);
        for t in [nm, nm_lo] {
            let hi = t * m;
            acc.add(hi);
            acc.add(t.mul_add(m, -hi));
        }
        acc.total().unwrap() / (n - 1.0)
    }
}

/// The same, the power sums in superaccumulators.
#[derive(Clone, Default)]
struct Power2 {
    n: u64,
    s1: SuperAcc,
    s2: SuperAcc,
}
impl Power2 {
    #[inline]
    fn add(&mut self, x: f64) {
        self.n += 1;
        self.s1.add(x);
        let hi = x * x;
        self.s2.add(hi);
        self.s2.add(x.mul_add(x, -hi));
    }
    fn merge(&mut self, o: &Power2) {
        self.n += o.n;
        self.s1.merge(&o.s1);
        self.s2.merge(&o.s2);
    }
    fn var(&self) -> f64 {
        let mut p = Power { n: self.n, ..Default::default() };
        self.s1.parts().into_iter().for_each(|x| p.s1.add(x));
        self.s2.parts().into_iter().for_each(|x| p.s2.add(x));
        p.var()
    }
}

fn splits(xs: &[f64], rng: &mut Rng, parts: usize) -> Vec<Vec<f64>> {
    // Morsels of 10 000 handed to `parts` workers at random.
    let mut out = vec![Vec::new(); parts];
    for chunk in xs.chunks(10_000) {
        out[(rng.next() % parts as u64) as usize].extend_from_slice(chunk);
    }
    out
}

fn main() {
    let n = 10_000_000;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let offset: Vec<f64> = (0..n).map(|_| 1e9 + rng.unit()).collect();
    let normal: Vec<f64> = (0..n).map(|_| rng.normal()).collect();
    for (name, xs, truth) in [("1e9 + U(0,1)", &offset, None), ("N(0, 1)", &normal, None::<f64>)] {
        let _ = truth;
        // The reference: exact power sums (checked against a two-pass exact below).
        let mut p = Power::default();
        xs.iter().for_each(|&x| p.add(x));
        let reference = p.var();
        // A two-pass exact check: mean exactly, then Σ(x−m)² exactly.
        let mut s = ExactSum::new();
        xs.iter().for_each(|&x| s.add(x));
        let m = s.total().unwrap() / xs.len() as f64;
        let mut s2 = ExactSum::new();
        for &x in xs.iter() {
            let d = x - m; // exact when m is near x (Sterbenz) — not always
            let hi = d * d;
            s2.add(hi);
            s2.add(d.mul_add(d, -hi));
        }
        let two_pass = s2.total().unwrap() / (xs.len() as f64 - 1.0);
        let mut q = Power2::default();
        xs.iter().for_each(|&x| q.add(x));
        println!("  superaccumulator: {:.17e}, {} ulps from the reference", q.var(), (q.var().to_bits() as i64 - reference.to_bits() as i64));
        let rel = |v: f64| ((v - reference) / reference).abs();
        let mut w = Welford::default();
        xs.iter().for_each(|&x| w.add(x));
        let mut yc = YoungsCramer::default();
        xs.iter().for_each(|&x| yc.add(x));
        println!("{name}: exact {reference:.17e} (two-pass {two_pass:.17e})");
        println!(
            "  relative error: naive {:.2e}, Welford {:.2e}, Youngs–Cramer {:.2e}",
            rel(naive(xs)),
            rel(w.var()),
            rel(yc.var())
        );
        // Reproducibility: 8 workers, morsels at random, 20 runs.
        let mut welford_results = std::collections::BTreeSet::new();
        let mut exact_results = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let parts = splits(xs, &mut rng, 8);
            let mut wm = Welford::default();
            let mut pm = Power::default();
            for part in &parts {
                let mut wp = Welford::default();
                part.iter().for_each(|&x| wp.add(x));
                wm.merge(&wp);
                let mut pp = Power::default();
                part.iter().for_each(|&x| pp.add(x));
                pm.merge(&pp);
            }
            welford_results.insert(wm.var().to_bits());
            exact_results.insert(pm.var().to_bits());
        }
        println!(
            "  20 runs, 8 workers: Welford+Chan {} distinct results, exact {} distinct",
            welford_results.len(),
            exact_results.len()
        );
        // Cost per value, best of 5.
        let time = |f: &dyn Fn() -> f64| {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                std::hint::black_box(f());
                best = best.min(t.elapsed().as_secs_f64());
            }
            best * 1e9 / xs.len() as f64
        };
        println!(
            "  ns a value: naive {:.2}, Welford {:.2}, Youngs–Cramer {:.2}, exact power sums {:.2}",
            time(&|| naive(xs)),
            time(&|| {
                let mut w = Welford::default();
                xs.iter().for_each(|&x| w.add(x));
                w.var()
            }),
            time(&|| {
                let mut y = YoungsCramer::default();
                xs.iter().for_each(|&x| y.add(x));
                y.var()
            }),
            time(&|| {
                let mut p = Power::default();
                xs.iter().for_each(|&x| p.add(x));
                p.var()
            }),
        );
        println!(
            "  ns a value: superaccumulator power sums {:.2}, superaccumulator sum alone {:.2}",
            time(&|| {
                let mut p = Power2::default();
                xs.iter().for_each(|&x| p.add(x));
                p.var()
            }),
            time(&|| {
                let mut p = SuperAcc::default();
                xs.iter().for_each(|&x| p.add(x));
                p.parts()[0]
            }),
        );
    }
}
