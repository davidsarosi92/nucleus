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
//! The result is rounded once, from the integer ([`BigInt::to_f64`]: its
//! top 53 bits, the next bit and whether any below it is set).

use crate::bigint::BigInt;

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

    /// Adds `x · y` exactly: the product of the two significands, 106
    /// bits in one 128-bit multiplication, added at its place in one step
    /// (unless the product is below 2⁻⁹⁶⁹: its bits below 2⁻¹⁰⁷⁵, the
    /// accumulator's unit, are then dropped).  An infinite or `NaN`
    /// product is added as one.
    #[inline]
    pub fn add_product(&mut self, x: f64, y: f64) {
        let prod = x * y;
        if !prod.is_finite() {
            self.add(prod);
            return;
        }
        let (Some((mx, ex)), Some((my, ey))) = (significand(x), significand(y)) else {
            return; // a zero
        };
        let mut p = u128::from(mx) * u128::from(my);
        // Its lowest bit's place, in units of 2⁻¹⁰⁷⁵.
        let mut at = ex + ey - 1075;
        if at < 0 {
            p >>= (-at).min(127);
            at = 0;
            if p == 0 {
                return;
            }
        }
        let (i, bit) = ((at >> 5) as usize, (at & 31) as u32);
        // p · 2^bit: up to 137 bits, five chunks.
        let lo = p << bit;
        let hi = if bit == 0 {
            0
        } else {
            (p >> (128 - bit)) as u64
        };
        let c = [
            (lo as u64 & 0xffff_ffff) as i64,
            ((lo >> 32) as u64 & 0xffff_ffff) as i64,
            ((lo >> 64) as u64 & 0xffff_ffff) as i64,
            ((lo >> 96) as u64) as i64,
            hi as i64,
        ];
        let used = (bit + 128 - p.leading_zeros()).div_ceil(32) as usize;
        self.cover(i, i + used);
        let at = i - self.lo;
        let w = &mut self.chunks[at..at + used];
        if (x < 0.0) != (y < 0.0) {
            w.iter_mut().zip(&c).for_each(|(w, c)| *w -= c);
        } else {
            w.iter_mut().zip(&c).for_each(|(w, c)| *w += c);
        }
        self.adds += 1;
        if self.adds == CARRY_EVERY {
            self.carry();
        }
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

    /// Each chunk brought into [−2³¹, 2³¹) by carrying into the next; a
    /// chunk is added above the top only when the top is out of that range
    /// (at chunk 66 the top keeps what is left: a double's sum never
    /// reaches past it).
    fn carry(&mut self) {
        let n = self.chunks.len();
        for i in 0..n.saturating_sub(1) {
            let c = (self.chunks[i] + (1 << 31)) >> 32;
            self.chunks[i] -= c << 32;
            self.chunks[i + 1] += c;
        }
        while let Some(&top) = self.chunks.last() {
            let c = (top + (1 << 31)) >> 32;
            if c == 0 || self.lo + self.chunks.len() == CHUNKS {
                break;
            }
            *self.chunks.last_mut().unwrap() -= c << 32;
            self.chunks.push(c);
        }
        self.adds = 0;
    }

    /// Adds everything `other` holds.
    pub fn merge(&mut self, other: &ExactSum) {
        if !other.chunks.is_empty() {
            // A chunk holds up to (adds + 1) · 2³² in magnitude, so the
            // two together are one more addition than both counts.
            if self.adds + other.adds >= CARRY_EVERY - 1 {
                self.carry();
            }
            let o = other.lo + other.chunks.len();
            self.cover(other.lo, o);
            let at = other.lo - self.lo;
            for (s, c) in self.chunks[at..].iter_mut().zip(&other.chunks) {
                *s += c;
            }
            self.adds += other.adds + 1;
            if self.adds >= CARRY_EVERY {
                self.carry();
            }
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
        let (v, exp) = self.exact()?;
        let x = v.to_f64(exp, false);
        x.is_finite().then_some(x)
    }

    /// The sum exactly, `v · 2^exp`; `None` when an infinity or `NaN` was
    /// added.
    pub fn exact(&self) -> Option<(BigInt, i64)> {
        if self.special.is_some() {
            return None;
        }
        Some((
            BigInt::from_signed_digits(&self.chunks),
            32 * self.lo as i64 - 1075,
        ))
    }

    /// The state as bytes, for [`ExactSum::from_bytes`] in another
    /// thread's accumulator: the first chunk's place, whether a special
    /// sum follows (and it), then the chunks carried — each but the top in
    /// 4 bytes, the top in 8 — without the zero chunks at either end.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + 4 * self.chunks.len());
        out.push(0);
        match self.special {
            Some(s) => {
                out.push(1);
                out.extend_from_slice(&s.to_le_bytes());
            }
            None => out.push(0),
        }
        let head = out.len();
        let mut lo = self.lo;
        let mut carry = 0i64;
        let n = self.chunks.len();
        for (i, &c) in self.chunks.iter().enumerate() {
            let v = c + carry;
            if i + 1 == n {
                // The top, and what a carry out of it leaves above.
                let mut v = v;
                while lo + (out.len() - head) / 4 + 1 < CHUNKS {
                    let up = (v + (1 << 31)) >> 32;
                    if up == 0 {
                        break;
                    }
                    out.extend_from_slice(&((v - (up << 32)) as i32).to_le_bytes());
                    v = up;
                }
                if v != 0 || out.len() > head {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                break;
            }
            carry = (v + (1 << 31)) >> 32;
            let d = (v - (carry << 32)) as i32;
            if d == 0 && out.len() == head {
                lo += 1;
            } else {
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
        // Zero chunks below the top, now the top's 8 bytes are known.
        out[0] = lo.min(CHUNKS) as u8;
        out
    }

    /// A state from [`ExactSum::to_bytes`]; `None` if malformed.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut s = ExactSum::new();
        s.merge_bytes(b)?;
        Some(s)
    }

    /// Adds a state [`ExactSum::to_bytes`] made, without building it;
    /// `None` (and nothing added) if malformed.
    pub fn merge_bytes(&mut self, b: &[u8]) -> Option<()> {
        let (&lo, rest) = b.split_first()?;
        let (&flag, rest) = rest.split_first()?;
        let (special, rest) = match flag {
            0 => (None, rest),
            1 => {
                let (s, rest) = rest.split_first_chunk::<8>()?;
                (Some(f64::from_le_bytes(*s)), rest)
            }
            _ => return None,
        };
        let lo = usize::from(lo);
        if !rest.is_empty() {
            let (digits, top) = rest.split_at_checked(rest.len().checked_sub(8)?)?;
            let (digits, tail) = digits.as_chunks::<4>();
            let to = lo + digits.len() + 1;
            if !tail.is_empty() || to > CHUNKS {
                return None;
            }
            if self.adds >= CARRY_EVERY - 1 {
                self.carry();
            }
            self.cover(lo, to);
            let at = lo - self.lo;
            let w = &mut self.chunks[at..at + digits.len() + 1];
            for (s, d) in w.iter_mut().zip(digits) {
                *s += i64::from(i32::from_le_bytes(*d));
            }
            w[digits.len()] += i64::from_le_bytes(top.try_into().ok()?);
            self.adds += 1;
        }
        if let Some(s) = special {
            self.special = Some(self.special.map_or(s, |t| t + s));
        }
        Some(())
    }
}

/// A finite double's significand and the place of its lowest bit (its
/// exponent field, 1 for a subnormal); `None` for a zero.
#[inline]
fn significand(x: f64) -> Option<(u64, i64)> {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let mant = bits & ((1u64 << 52) - 1);
    match exp {
        0 if mant == 0 => None,
        0 => Some((mant, 1)),
        _ => Some((mant | 1 << 52, exp)),
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

    /// A state through its bytes, alone and into another, is the same sum:
    /// negative, cancelled to zero, at the top of the range, special.
    #[test]
    fn states_survive_their_bytes() {
        let cases: &[&[f64]] = &[
            &[],
            &[-1.5],
            &[1.0, -1.0],
            &[1e300, -1e-300, 3.0],
            &[f64::MAX, f64::MAX, -f64::MAX],
            &[-f64::MAX, -f64::MAX, 5e-324],
            &[5e-324, -1e-320],
            &[f64::NAN, 1.0],
            &[-(2f64.powi(31)), 2f64.powi(-1000)],
        ];
        for xs in cases {
            let mut a = ExactSum::new();
            xs.iter().for_each(|&x| a.add(x));
            let bytes = a.to_bytes();
            let b = ExactSum::from_bytes(&bytes).unwrap();
            let (want, got) = (a.total(), b.total());
            assert!(want == got || want.unwrap().is_nan() && got.unwrap().is_nan());
            let mut c = ExactSum::new();
            c.add(0.25);
            c.add(-1e200);
            c.merge_bytes(&bytes).unwrap();
            let mut d = ExactSum::new();
            [0.25, -1e200]
                .iter()
                .chain(xs.iter())
                .for_each(|&x| d.add(x));
            let (want, got) = (d.total(), c.total());
            assert!(want == got || want.unwrap().is_nan() && got.unwrap().is_nan());
        }
        assert!(ExactSum::from_bytes(&[0, 2]).is_none());
        assert!(ExactSum::from_bytes(&[66, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]).is_none());
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
        // Random products of any sign, 2⁻⁴²³ … 2³⁷⁷, against h + l from a
        // fused multiply-add (exact here: no error below the subnormals).
        let mut seed = 0x853c_49e6_748f_ea9bu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut p, mut q) = (ExactSum::new(), ExactSum::new());
        for _ in 0..20_000 {
            let mut one = || {
                let x = f64::from_bits(next() & !(0xfffu64 << 52) | ((next() % 800 + 600) << 52));
                if next() % 2 == 0 { x } else { -x }
            };
            let (x, y) = (one(), one());
            p.add_product(x, y);
            let h = x * y;
            q.add(h);
            q.add(x.mul_add(y, -h));
        }
        let ((a, ea), (b, eb)) = (p.exact().unwrap(), q.exact().unwrap());
        let e = ea.min(eb);
        assert!(
            a.shl((ea - e) as u64)
                .cmp_value(&b.shl((eb - e) as u64))
                .is_eq()
        );
        assert_eq!(p.total(), q.total());
        // A subnormal factor, a product far below the doubles' least.
        let mut p = ExactSum::new();
        p.add_product(5e-324, 2f64.powi(60));
        assert_eq!(p.total(), Some(5e-324 * 2f64.powi(60)));
        p.add_product(-3.0, 1.5);
        assert_eq!(p.total(), Some(5e-324 * 2f64.powi(60) - 4.5));
    }
}
