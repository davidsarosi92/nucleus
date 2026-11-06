//! Exact sums of doubles (PLAN.md §M14.20 V4, §M14.27 S1): the sum of
//! every value added, rounded once at the end — whatever order the values
//! came in, so a sum split across threads and combined is the sum a single
//! pass gives, bit for bit.
//!
//! The one exact accumulator, Neal's small superaccumulator ("Fast exact
//! summation using small and large superaccumulators", 2015): a double is
//! a 53-bit integer times a power of two, so a sum of doubles is an integer
//! in units of 2⁻¹⁰⁷⁵, kept here in signed 64-bit chunks of 32 bits each —
//! an addition is three integer additions, whatever the magnitudes, and a
//! subtraction the same (a sliding window removes a row exactly).  Only
//! the chunks a sum has touched are kept (a window, grown as values come),
//! so a state of values of like magnitude is a few words, not Neal's 67.
//! The result is rounded once, from the integer: its top 53 bits, the next
//! bit and whether any below it is set.

/// Chunks of 32 bits: a chunk holds additions of up to 2³² each, so
/// `CARRY_EVERY` of them cannot overflow its 64 bits.
const CARRY_EVERY: u32 = 1 << 30;

/// A double's exponent field divided by 32, plus the two chunks its
/// 53-bit significand can reach: 0 ..= 65.
const CHUNKS: usize = 67;

/// A sum of doubles kept exactly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExactSum {
    /// The chunks from `lo`: chunk `lo + i` in units of 2^(32(lo+i) − 1075).
    chunks: Vec<i64>,
    lo: usize,
    /// Additions since the chunks were last carried.
    adds: u32,
    /// The sum of the infinities and `NaN`s added (`None`: none).
    special: Option<f64>,
}

impl ExactSum {
    /// An empty sum.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `x`.
    #[inline]
    pub fn add(&mut self, x: f64) {
        self.put(x, false);
    }

    /// Subtracts `x` (exactly: a window frame's row leaving it).
    #[inline]
    pub fn sub(&mut self, x: f64) {
        self.put(x, true);
    }

    /// Adds `x · y` exactly: its rounded product and the product's error,
    /// which a fused multiply-add gives exactly (unless the error is below
    /// the smallest double, 2⁻¹⁰⁷⁴ — then it is that close).
    #[inline]
    pub fn add_product(&mut self, x: f64, y: f64) {
        let hi = x * y;
        if !hi.is_finite() {
            self.add(hi);
            return;
        }
        self.add(hi);
        self.add(x.mul_add(y, -hi));
    }

    #[inline]
    fn put(&mut self, x: f64, negate: bool) {
        let bits = x.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as usize;
        if exp == 0x7ff {
            let x = if negate { -x } else { x };
            self.special = Some(self.special.map_or(x, |s| s + x));
            return;
        }
        let mut mant = bits & ((1u64 << 52) - 1);
        let exp = if exp == 0 {
            if mant == 0 {
                return;
            }
            1
        } else {
            mant |= 1 << 52;
            exp
        };
        // The significand at its place: bits 32·i … of the sum.
        let v = u128::from(mant) << (exp & 31);
        let i = exp >> 5;
        let c = [
            (v as u64 & 0xffff_ffff) as i64,
            ((v >> 32) as u64 & 0xffff_ffff) as i64,
            (v >> 64) as i64,
        ];
        self.cover(i, i + 3);
        let at = i - self.lo;
        let w = &mut self.chunks[at..at + 3];
        if (bits >> 63 != 0) != negate {
            w[0] -= c[0];
            w[1] -= c[1];
            w[2] -= c[2];
        } else {
            w[0] += c[0];
            w[1] += c[1];
            w[2] += c[2];
        }
        self.adds += 1;
        if self.adds == CARRY_EVERY {
            self.carry();
        }
    }

    /// Grows the window to hold chunks `from .. to`.
    #[inline]
    fn cover(&mut self, from: usize, to: usize) {
        if self.chunks.is_empty() {
            self.lo = from;
            self.chunks.resize(to - from, 0);
            return;
        }
        if from < self.lo {
            let more = self.lo - from;
            self.chunks.splice(0..0, std::iter::repeat_n(0, more));
            self.lo = from;
        }
        if to > self.lo + self.chunks.len() {
            self.chunks.resize(to - self.lo, 0);
        }
    }

    /// Each chunk brought into [−2³¹, 2³¹) by carrying into the next (the
    /// top chunk keeps what is left: a double's sum never reaches past it).
    fn carry(&mut self) {
        let n = self.chunks.len();
        if n > 0 && self.lo + n < CHUNKS {
            self.chunks.push(0);
        }
        let n = self.chunks.len();
        for i in 0..n.saturating_sub(1) {
            let c = (self.chunks[i] + (1 << 31)) >> 32;
            self.chunks[i] -= c << 32;
            self.chunks[i + 1] += c;
        }
        self.adds = 0;
    }

    /// Adds everything `other` holds.
    pub fn merge(&mut self, other: &ExactSum) {
        if !other.chunks.is_empty() {
            self.carry();
            let mut o = other.clone();
            o.carry();
            self.cover(o.lo, o.lo + o.chunks.len());
            let at = o.lo - self.lo;
            for (i, c) in o.chunks.iter().enumerate() {
                self.chunks[at + i] += c;
            }
            self.carry();
        }
        if let Some(s) = other.special {
            self.special = Some(self.special.map_or(s, |t| t + s));
        }
    }

    /// The sum, correctly rounded (to nearest, ties to even); `None` when
    /// it overflowed (finite values whose sum is beyond a double).
    pub fn total(&self) -> Option<f64> {
        if let Some(s) = self.special {
            return Some(s);
        }
        let mut s = self.clone();
        s.carry();
        let mut d = s.chunks;
        // The sign is the top nonzero chunk's (the ones below it, each in
        // [−2³¹, 2³¹), add up to less than one of its units).
        let Some(top) = d.iter().rposition(|&c| c != 0) else {
            return Some(0.0);
        };
        let negative = d[top] < 0;
        if negative {
            d.iter_mut().for_each(|c| *c = -*c);
        }
        // Digits in [0, 2³²): borrows carried up (the top stays positive).
        for i in 0..top {
            if d[i] < 0 {
                d[i] += 1 << 32;
                d[i + 1] -= 1;
            }
        }
        d.truncate(top + 1);
        let top = d.iter().rposition(|&c| c != 0).expect("positive");
        if d[top] >> 32 != 0 {
            return None;
        }
        let lo = s.lo;
        // Bit `i` of the magnitude, in units of 2⁻¹⁰⁷⁵.
        let bit = |i: usize| -> u64 {
            let (k, b) = (i / 32, i % 32);
            if k < lo || k - lo >= d.len() {
                0
            } else {
                (d[k - lo] as u64 >> b) & 1
            }
        };
        let high = 32 * (lo + top) + 63 - (d[top] as u64).leading_zeros() as usize;
        // The significand: 53 bits from the top, or fewer down to the
        // subnormals' grid (2⁻¹⁰⁷⁴: units of 2, so at least one bit off).
        let mut shift = high.saturating_sub(52).max(1);
        let mut m = 0u64;
        for i in (shift..=high).rev() {
            m = (m << 1) | bit(i);
        }
        let round = bit(shift - 1) == 1;
        let sticky = (lo * 32..shift - 1).any(|i| bit(i) == 1);
        if round && (sticky || m & 1 == 1) {
            m += 1;
            if m == 1 << 53 {
                m >>= 1;
                shift += 1;
            }
        }
        // A normal double's exponent field is `shift` itself: the value is
        // m · 2^(shift − 1075), m in [2⁵², 2⁵³).
        let bits = if m >> 52 == 0 {
            m
        } else if shift >= 0x7ff {
            return None;
        } else {
            ((shift as u64) << 52) | (m & ((1 << 52) - 1))
        };
        Some(f64::from_bits(bits | (u64::from(negative) << 63)))
    }

    /// The state as bytes, for [`ExactSum::from_bytes`] in another
    /// thread's accumulator: the first chunk's place, the chunks, and the
    /// special sum.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.clone();
        s.carry();
        let mut out = Vec::with_capacity(10 + 8 * s.chunks.len());
        out.push(s.lo as u8);
        out.push(u8::from(s.special.is_some()));
        out.extend_from_slice(&s.special.unwrap_or(0.0).to_le_bytes());
        for c in &s.chunks {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    /// A state from [`ExactSum::to_bytes`]; `None` if malformed.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let (head, rest) = b.split_at_checked(10)?;
        let (chunks, tail) = rest.as_chunks::<8>();
        let lo = usize::from(head[0]);
        if !tail.is_empty() || lo + chunks.len() > CHUNKS {
            return None;
        }
        Some(ExactSum {
            chunks: chunks.iter().map(|c| i64::from_le_bytes(*c)).collect(),
            lo,
            adds: 0,
            special: (head[1] == 1).then(|| f64::from_le_bytes(head[2..10].try_into().unwrap())),
        })
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
        assert_eq!(sum(&[1e308, 1e308, -1e308]), Some(1e308));
        assert_eq!(sum(&[f64::MAX, f64::MAX, -f64::MAX]), Some(f64::MAX));
        assert_eq!(sum(&[f64::INFINITY, 1.0]), Some(f64::INFINITY));
        assert!(sum(&[f64::INFINITY, f64::NEG_INFINITY]).unwrap().is_nan());
        assert_eq!(sum(&[]), Some(0.0));
        assert_eq!(sum(&[5e-324, 5e-324]), Some(1e-323));
        assert_eq!(sum(&[-0.0]), Some(0.0));
        // Ties rounded to even, decided by bits far below the result's.
        assert_eq!(
            sum(&[1.0, 2f64.powi(-53), 2f64.powi(-1000)]),
            Some(1.0 + f64::EPSILON)
        );
        assert_eq!(sum(&[1.0, 2f64.powi(-53)]), Some(1.0));
        assert_eq!(
            sum(&[1.0, 2f64.powi(-53), 5e-324]),
            Some(1.0 + f64::EPSILON)
        );
        assert_eq!(
            sum(&[-1.0, -(2f64.powi(-53)), -5e-324]),
            Some(-1.0 - f64::EPSILON)
        );
        let xs: Vec<f64> = (0..1000)
            .map(|i| (i as f64 * 0.37).sin() * 10f64.powi(i % 40 - 20))
            .collect();
        let mut rev = xs.clone();
        rev.reverse();
        assert_eq!(sum(&xs), sum(&rev));
        for split in [1, 400, 999] {
            let (mut a, mut b) = (ExactSum::new(), ExactSum::new());
            xs[..split].iter().for_each(|&x| a.add(x));
            xs[split..].iter().for_each(|&x| b.add(x));
            a.merge(&b);
            assert_eq!(a.total(), sum(&xs));
            let b = ExactSum::from_bytes(&a.to_bytes()).unwrap();
            assert_eq!(b.total(), sum(&xs));
        }
    }

    /// Against exact rational arithmetic: integers in units of 2⁻¹⁰⁷⁴ for
    /// values within a window of magnitudes, summed in `i128`... beyond its
    /// range, against a two-pass reference: the sorted sum of the values
    /// and of what each addition lost.
    #[test]
    fn sums_match_an_integer_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            // Values k · 2^e, |k| < 2^20, e in [−40, 40]: exact in i128
            // units of 2^−40.
            let xs: Vec<(i128, f64)> = (0..50)
                .map(|_| {
                    let k = (next() % (1 << 21)) as i128 - (1 << 20);
                    let e = (next() % 81) as i32 - 40;
                    (k << (e + 40), k as f64 * 2f64.powi(e))
                })
                .collect();
            let exact: i128 = xs.iter().map(|(u, _)| u).sum();
            let want = exact as f64 * 2f64.powi(-40);
            // `exact as f64` rounds once (to nearest even): the reference.
            let got = sum(&xs.iter().map(|(_, x)| *x).collect::<Vec<_>>());
            assert_eq!(got, Some(want));
        }
    }

    /// Shewchuk's summation with Python's `math.fsum` rounding: the
    /// reference the accumulator replaced.  `None` where a partial
    /// overflows.
    fn fsum(xs: &[f64]) -> Option<f64> {
        let mut p: Vec<f64> = Vec::new();
        for &v in xs {
            let mut x = v;
            let mut i = 0;
            for j in 0..p.len() {
                let mut y = p[j];
                if x.abs() < y.abs() {
                    std::mem::swap(&mut x, &mut y);
                }
                let hi = x + y;
                if !hi.is_finite() {
                    return None;
                }
                let lo = y - (hi - x);
                if lo != 0.0 {
                    p[i] = lo;
                    i += 1;
                }
                x = hi;
            }
            p.truncate(i);
            p.push(x);
        }
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
                lo = y - (hi - x);
                if lo != 0.0 {
                    break;
                }
            }
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

    /// Any finite doubles — subnormals, the largest, cancellations, ties —
    /// against Shewchuk's summation, in any order and split.
    #[test]
    fn sums_match_shewchuk_over_every_magnitude() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..2000 {
            let n = (next() % 40) as usize + 1;
            let mut xs: Vec<f64> = (0..n)
                .map(|_| {
                    let r = next();
                    match r % 4 {
                        // Any finite bits.
                        0 => {
                            let x = f64::from_bits(next());
                            if x.is_finite() { x } else { 1.0 }
                        }
                        // Near one magnitude (round-to-even cases).
                        1 => {
                            (1.0 + (next() % 1024) as f64 * f64::EPSILON / 2.0)
                                * 2f64.powi((next() % 8) as i32)
                        }
                        // Subnormals and the smallest normals.
                        2 => f64::from_bits(next() % (1 << 54)),
                        // The largest.
                        _ => f64::MAX / (1 + next() % 4) as f64,
                    }
                    .copysign(if r & 16 == 0 { 1.0 } else { -1.0 })
                })
                .collect();
            if round % 3 == 0 {
                // Cancel all but a few, so the result is small.
                let back: Vec<f64> = xs.iter().take(n / 2).map(|x| -x).collect();
                xs.extend(back);
            }
            let want = fsum(&xs);
            let got = sum(&xs);
            // An overflowing partial is not an overflowing sum: Shewchuk's
            // `None` is only an answer when the accumulator agrees or the
            // exact sum is out of range.
            if want.is_some() {
                assert_eq!(got, want, "{xs:?}");
            }
            let split = (next() as usize) % xs.len();
            let (mut a, mut b) = (ExactSum::new(), ExactSum::new());
            xs[..split].iter().for_each(|&x| a.add(x));
            xs[split..].iter().rev().for_each(|&x| b.add(x));
            b.merge(&a);
            assert_eq!(b.total(), got);
        }
    }

    #[test]
    fn subtraction_and_products_are_exact() {
        let mut s = ExactSum::new();
        s.add(1e9 + 0.25);
        s.add(3.0);
        s.sub(1e9 + 0.25);
        assert_eq!(s.total(), Some(3.0));
        let mut s = ExactSum::new();
        let x = 1e9 + 0.1;
        s.add_product(x, x);
        s.sub(x * x);
        // What the rounded square lost, exactly.
        assert_eq!(s.total(), Some(x.mul_add(x, -(x * x))));
        let mut s = ExactSum::new();
        s.add_product(1e300, 1e300);
        assert_eq!(s.total(), Some(f64::INFINITY));
    }
}
