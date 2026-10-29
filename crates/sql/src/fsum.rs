//! Exact sums of doubles (PLAN.md §M14.20 V4): the sum of every value
//! added, rounded once at the end — whatever order the values came in, so
//! a sum split across threads and combined is the sum a single pass gives.
//! Shewchuk's algorithm (as Python's `math.fsum`): the running sum is kept
//! as a few non-overlapping doubles, each addition exact.

/// A sum of doubles kept exactly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExactSum {
    /// Non-overlapping partial sums, increasing in magnitude.
    partials: Vec<f64>,
    /// The sum of the infinities and `NaN`s added (`None`: none).
    special: Option<f64>,
    /// A partial sum overflowed.
    overflow: bool,
}

impl ExactSum {
    /// An empty sum.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `x`.
    pub fn add(&mut self, mut x: f64) {
        if !x.is_finite() {
            self.special = Some(self.special.map_or(x, |s| s + x));
            return;
        }
        let mut i = 0;
        for j in 0..self.partials.len() {
            let mut y = self.partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            if !hi.is_finite() {
                self.overflow = true;
                return;
            }
            let lo = y - (hi - x);
            if lo != 0.0 {
                self.partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        self.partials.truncate(i);
        self.partials.push(x);
    }

    /// Adds everything `other` holds.
    pub fn merge(&mut self, other: &ExactSum) {
        for &p in &other.partials {
            self.add(p);
        }
        if let Some(s) = other.special {
            self.add(s);
        }
        self.overflow |= other.overflow;
    }

    /// The sum, correctly rounded; `None` when it overflowed (finite values
    /// whose sum is beyond a double).
    pub fn total(&self) -> Option<f64> {
        if let Some(s) = self.special {
            return Some(s);
        }
        if self.overflow {
            return None;
        }
        let p = &self.partials;
        let mut n = p.len();
        let mut hi = 0.0;
        if n > 0 {
            n -= 1;
            hi = p[n];
            let mut lo = 0.0;
            while n > 0 {
                let x = hi;
                n -= 1;
                let y = p[n];
                hi = x + y;
                let yr = hi - x;
                lo = y - yr;
                if lo != 0.0 {
                    break;
                }
            }
            // Round half to even across the partials left.
            if n > 0 && ((lo < 0.0 && p[n - 1] < 0.0) || (lo > 0.0 && p[n - 1] > 0.0)) {
                let y = lo * 2.0;
                let x = hi + y;
                if y == x - hi {
                    hi = x;
                }
            }
        }
        hi.is_finite().then_some(hi)
    }

    /// The state as values: the partials, the special sum, whether it
    /// overflowed (for [`ExactSum::from_parts`]).
    pub fn parts(&self) -> (Vec<f64>, Option<f64>, bool) {
        (self.partials.clone(), self.special, self.overflow)
    }

    /// A state from [`ExactSum::parts`].
    pub fn from_parts(partials: Vec<f64>, special: Option<f64>, overflow: bool) -> Self {
        ExactSum {
            partials,
            special,
            overflow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExactSum;

    fn sum(xs: &[f64]) -> Option<f64> {
        let mut s = ExactSum::new();
        xs.iter().for_each(|&x| s.add(x));
        s.total()
    }

    #[test]
    fn sums_are_exact_and_order_free() {
        assert_eq!(sum(&[0.1; 10]), Some(1.0));
        assert_eq!(sum(&[1e100, 1.0, -1e100]), Some(1.0));
        assert_eq!(sum(&[1e308, 1e308]), None);
        assert_eq!(sum(&[f64::INFINITY, 1.0]), Some(f64::INFINITY));
        assert!(sum(&[f64::INFINITY, f64::NEG_INFINITY]).unwrap().is_nan());
        assert_eq!(sum(&[]), Some(0.0));
        let xs: Vec<f64> = (0..1000)
            .map(|i| (i as f64 * 0.37).sin() * 10f64.powi(i % 20))
            .collect();
        let mut rev = xs.clone();
        rev.reverse();
        assert_eq!(sum(&xs), sum(&rev));
        let (mut a, mut b) = (ExactSum::new(), ExactSum::new());
        xs[..400].iter().for_each(|&x| a.add(x));
        xs[400..].iter().for_each(|&x| b.add(x));
        a.merge(&b);
        assert_eq!(a.total(), sum(&xs));
    }
}
