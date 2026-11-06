//! Signed integers of any size (PLAN.md §M14.27 S2): what exact statistics
//! compute their results from once the rows are summed — a variance's
//! numerator `n·Σx² − (Σx)²` from exact power sums, its quotient rounded
//! once, a square root to a decimal scale.  Small and slow by design: it
//! runs once a group, after the rows, never a row at a time.
//!
//! The magnitude in base 2³², least significant first, without leading
//! zeros (zero is empty and never negative).

use std::cmp::Ordering;

/// A signed integer of any size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

impl BigInt {
    /// Zero.
    pub fn zero() -> Self {
        BigInt::default()
    }

    /// From an `i128`.
    pub fn from_i128(v: i128) -> Self {
        let mut b = BigInt::from_u128(v.unsigned_abs());
        b.neg = v < 0 && !b.mag.is_empty();
        b
    }

    /// From a `u128`.
    pub fn from_u128(mut v: u128) -> Self {
        let mut mag = Vec::new();
        while v != 0 {
            mag.push(v as u32);
            v >>= 32;
        }
        BigInt { neg: false, mag }
    }

    /// From two's-complement 64-bit words, least significant first.
    pub fn from_words(words: &[u64]) -> Self {
        let neg = words.last().is_some_and(|w| w >> 63 == 1);
        let mut w: Vec<u64> = words.to_vec();
        if neg {
            // The magnitude: the words negated.
            let mut carry = true;
            for x in &mut w {
                let (y, c) = (!*x).overflowing_add(u64::from(carry));
                *x = y;
                carry = c;
            }
        }
        let mut mag: Vec<u32> = w
            .iter()
            .flat_map(|x| [*x as u32, (x >> 32) as u32])
            .collect();
        trim(&mut mag);
        BigInt {
            neg: neg && !mag.is_empty(),
            mag,
        }
    }

    /// From signed digits in base 2³² (each any `i64`), least significant
    /// first: `Σ dᵢ · 2^(32i)`.
    pub fn from_signed_digits(d: &[i64]) -> Self {
        // Carried into digits of [0, 2³²) and a signed carry out of the
        // top, in one pass; a negative total is then negated.
        let mut mag = Vec::with_capacity(d.len() + 2);
        let mut carry: i128 = 0;
        for &x in d {
            let t = i128::from(x) + carry;
            mag.push(t as u32); // t mod 2³²
            carry = t >> 32;
        }
        // What is left: a (small) signed number above the digits.
        while carry != 0 && carry != -1 {
            mag.push(carry as u32);
            carry >>= 32;
        }
        let neg = carry == -1;
        if neg {
            // Two's complement over the digits, with the top all ones.
            let mut c = true;
            for x in &mut mag {
                let (y, o) = (!*x).overflowing_add(u32::from(c));
                *x = y;
                c = o;
            }
            // All digits zero: the magnitude is 2^(32·len).
            if c {
                mag.push(1);
            }
        }
        trim(&mut mag);
        BigInt {
            neg: neg && !mag.is_empty(),
            mag,
        }
    }

    /// Whether it is zero.
    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    /// Whether it is below zero.
    pub fn is_negative(&self) -> bool {
        self.neg
    }

    /// Its negation.
    pub fn negated(mut self) -> Self {
        self.neg = !self.neg && !self.mag.is_empty();
        self
    }

    /// Its absolute value.
    pub fn abs(mut self) -> Self {
        self.neg = false;
        self
    }

    /// Its value when it fits an `i128`.
    pub fn to_i128(&self) -> Option<i128> {
        if self.mag.len() > 4 {
            return None;
        }
        let m = self
            .mag
            .iter()
            .rev()
            .fold(0u128, |a, &d| (a << 32) | u128::from(d));
        if self.neg {
            0i128.checked_sub_unsigned(m)
        } else {
            i128::try_from(m).ok()
        }
    }

    /// The number of bits of its magnitude (0 for zero).
    pub fn bits(&self) -> u64 {
        match self.mag.last() {
            None => 0,
            Some(&t) => 32 * (self.mag.len() as u64 - 1) + u64::from(32 - t.leading_zeros()),
        }
    }

    /// Bit `i` of its magnitude.
    fn bit(&self, i: u64) -> bool {
        let k = (i / 32) as usize;
        k < self.mag.len() && (self.mag[k] >> (i % 32)) & 1 == 1
    }

    /// The magnitude's bits from `i` up (at most 64 of them: those up to
    /// the top, which callers keep within 64 of `i`).
    fn bits_at(&self, i: u64) -> u64 {
        let (k, sh) = ((i / 32) as usize, i % 32);
        let limb = |j: usize| u128::from(*self.mag.get(j).unwrap_or(&0));
        let w = limb(k) | (limb(k + 1) << 32) | (limb(k + 2) << 64) | (limb(k + 3) << 96);
        (w >> sh) as u64
    }

    /// Whether any bit of its magnitude below `i` is set.
    fn any_below(&self, i: u64) -> bool {
        let k = (i / 32) as usize;
        self.mag.iter().take(k.min(self.mag.len())).any(|&d| d != 0)
            || (k < self.mag.len() && self.mag[k] & ((1u32 << (i % 32)) - 1) != 0)
    }

    /// `self + other`.
    pub fn add(&self, other: &BigInt) -> BigInt {
        if self.neg == other.neg {
            return BigInt {
                neg: self.neg,
                mag: add_mag(&self.mag, &other.mag),
            };
        }
        match cmp_mag(&self.mag, &other.mag) {
            Ordering::Equal => BigInt::zero(),
            Ordering::Greater => BigInt {
                neg: self.neg,
                mag: sub_mag(&self.mag, &other.mag),
            },
            Ordering::Less => BigInt {
                neg: other.neg,
                mag: sub_mag(&other.mag, &self.mag),
            },
        }
    }

    /// `self − other`.
    pub fn sub(&self, other: &BigInt) -> BigInt {
        self.add(&other.clone().negated())
    }

    /// `self · other`.
    pub fn mul(&self, other: &BigInt) -> BigInt {
        if self.is_zero() || other.is_zero() {
            return BigInt::zero();
        }
        let mut out = vec![0u32; self.mag.len() + other.mag.len()];
        for (i, &a) in self.mag.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.mag.iter().enumerate() {
                let t = u64::from(a) * u64::from(b) + u64::from(out[i + j]) + carry;
                out[i + j] = t as u32;
                carry = t >> 32;
            }
            out[i + other.mag.len()] = carry as u32;
        }
        trim(&mut out);
        BigInt {
            neg: self.neg != other.neg,
            mag: out,
        }
    }

    /// `self · 10^k`.
    pub fn mul_pow10(&self, k: u32) -> BigInt {
        let mut b = self.clone();
        for _ in 0..k / 9 {
            b.mul_small(1_000_000_000);
        }
        b.mul_small(10u32.pow(k % 9));
        b
    }

    fn mul_small(&mut self, m: u32) {
        let mut carry = 0u64;
        for d in &mut self.mag {
            let t = u64::from(*d) * u64::from(m) + carry;
            *d = t as u32;
            carry = t >> 32;
        }
        if carry != 0 {
            self.mag.push(carry as u32);
        }
        trim(&mut self.mag);
        if self.mag.is_empty() {
            self.neg = false;
        }
    }

    /// `self · 2^k`.
    pub fn shl(&self, k: u64) -> BigInt {
        if self.is_zero() {
            return BigInt::zero();
        }
        let (words, bits) = ((k / 32) as usize, k % 32);
        let mut mag = vec![0u32; words];
        let mut carry = 0u32;
        for &d in &self.mag {
            if bits == 0 {
                mag.push(d);
            } else {
                mag.push((d << bits) | carry);
                carry = d >> (32 - bits);
            }
        }
        if carry != 0 {
            mag.push(carry);
        }
        BigInt { neg: self.neg, mag }
    }

    /// The magnitude divided by `d` (not zero): the quotient's magnitude
    /// and the remainder, `|self| = q·d + r`.
    pub fn divmod_mag(&self, d: u128) -> (BigInt, u128) {
        assert!(d != 0, "division by zero");
        if d >> 96 == 0 {
            // A digit at a time: the remainder stays below 2⁹⁶.
            let mut q = vec![0u32; self.mag.len()];
            let mut r = 0u128;
            for i in (0..self.mag.len()).rev() {
                let t = (r << 32) | u128::from(self.mag[i]);
                q[i] = (t / d) as u32;
                r = t % d;
            }
            trim(&mut q);
            return (BigInt { neg: false, mag: q }, r);
        }
        // A bit at a time (divisors this wide are not met in practice).
        let mut q = BigInt::zero();
        let mut r = 0u128;
        for i in (0..self.bits()).rev() {
            let top = r >> 127;
            r = (r << 1) | u128::from(self.bit(i));
            q = q.shl(1);
            if top == 1 || r >= d {
                r = r.wrapping_sub(d);
                q = q.add(&BigInt::from_u128(1));
            }
        }
        (q, r)
    }

    /// `self / d` rounded half away from zero (PostgreSQL's numeric
    /// division).
    pub fn div_round(&self, d: u128) -> BigInt {
        let (q, r) = self.divmod_mag(d);
        // r ≥ d − r: half or more.
        let q = if r >= d - r {
            q.add(&BigInt::from_u128(1))
        } else {
            q
        };
        if self.neg { q.negated() } else { q }
    }

    /// The floor of the square root of a non-negative value.
    pub fn isqrt(&self) -> BigInt {
        assert!(!self.neg, "the square root of a negative");
        if self.is_zero() {
            return BigInt::zero();
        }
        // Newton's iteration from above, in exact integers.
        let mut x = BigInt::zero()
            .add(&BigInt::from_u128(1))
            .shl(self.bits().div_ceil(2));
        loop {
            let (q, _) = self.div_big(&x);
            let y = x.add(&q).half();
            if cmp_mag(&y.mag, &x.mag) != Ordering::Less {
                return x;
            }
            x = y;
        }
    }

    fn half(&self) -> BigInt {
        let mut mag = self.mag.clone();
        let mut carry = 0u32;
        for d in mag.iter_mut().rev() {
            let n = (*d >> 1) | (carry << 31);
            carry = *d & 1;
            *d = n;
        }
        trim(&mut mag);
        BigInt { neg: self.neg, mag }
    }

    /// The magnitudes' quotient and remainder: Knuth's Algorithm D
    /// (TAOCP vol. 2, 4.3.1; as Hacker's Delight's `divmnu`), a digit of
    /// 32 bits at a time.
    pub fn div_big(&self, d: &BigInt) -> (BigInt, BigInt) {
        assert!(!d.is_zero(), "a division by zero");
        if let Some(small) = d.to_i128().map(i128::unsigned_abs).filter(|v| v >> 96 == 0) {
            let (q, r) = self.divmod_mag(small);
            return (q, BigInt::from_u128(r));
        }
        let (u, v) = (&self.mag, &d.mag);
        if cmp_mag(u, v) == Ordering::Less {
            return (BigInt::zero(), self.clone().abs());
        }
        let (m, n) = (u.len(), v.len());
        // Normalized: the divisor's top bit set.
        let sh = v[n - 1].leading_zeros();
        let shift = |x: &[u32], extra: bool| -> Vec<u32> {
            let mut out = Vec::with_capacity(x.len() + 1);
            let mut carry = 0u32;
            for &w in x {
                out.push((w << sh) | carry);
                carry = if sh == 0 { 0 } else { w >> (32 - sh) };
            }
            if extra {
                out.push(carry);
            }
            out
        };
        let vn = shift(v, false);
        let mut un = shift(u, true);
        let mut q = vec![0u32; m - n + 1];
        const B: u64 = 1 << 32;
        for j in (0..=m - n).rev() {
            let top = (u64::from(un[j + n]) << 32) | u64::from(un[j + n - 1]);
            let mut qhat = top / u64::from(vn[n - 1]);
            let mut rhat = top % u64::from(vn[n - 1]);
            while qhat >= B || qhat * u64::from(vn[n - 2]) > (rhat << 32) + u64::from(un[j + n - 2])
            {
                qhat -= 1;
                rhat += u64::from(vn[n - 1]);
                if rhat >= B {
                    break;
                }
            }
            // un[j ..= j + n] −= qhat · vn.
            let mut k: i64 = 0;
            for i in 0..n {
                let p = qhat * u64::from(vn[i]);
                let t = i64::from(un[i + j]) - k - (p & 0xffff_ffff) as i64;
                un[i + j] = t as u32;
                k = (p >> 32) as i64 - (t >> 32);
            }
            let t = i64::from(un[j + n]) - k;
            un[j + n] = t as u32;
            q[j] = qhat as u32;
            if t < 0 {
                // One too many: add the divisor back.
                q[j] = q[j].wrapping_sub(1);
                let mut c = 0u64;
                for i in 0..n {
                    let t = u64::from(un[i + j]) + u64::from(vn[i]) + c;
                    un[i + j] = t as u32;
                    c = t >> 32;
                }
                un[j + n] = un[j + n].wrapping_add(c as u32);
            }
        }
        // The remainder, shifted back.
        let mut r: Vec<u32> = (0..n)
            .map(|i| {
                if sh == 0 {
                    un[i]
                } else {
                    (un[i] >> sh) | (un[i + 1] << (32 - sh))
                }
            })
            .collect();
        trim(&mut q);
        trim(&mut r);
        (BigInt { neg: false, mag: q }, BigInt { neg: false, mag: r })
    }

    /// `self · 2^exp`, correctly rounded to a double (to nearest, ties to
    /// even; `±∞` beyond the largest).  `sticky`: the exact value has
    /// more, nonzero bits below `self`'s (a quotient's remainder), which
    /// decide ties.
    pub fn to_f64(&self, exp: i64, sticky: bool) -> f64 {
        if self.is_zero() {
            // (Callers scale a quotient to 55 bits or more first, so a
            // zero with a remainder does not reach here.)
            return 0.0;
        }
        let sign = u64::from(self.neg) << 63;
        // The value's top bit is at 2^(high + exp); a double's unit in the
        // last place there is 2^(high + exp − 52), at least 2⁻¹⁰⁷⁴.
        let high = self.bits() as i64 - 1;
        let lsb = (high + exp - 52).max(-1074) - exp;
        let (mut m, round, below) = if lsb <= 0 {
            // Every bit kept: shifted up, nothing rounded off.
            let m = self.mag_u128() << (-lsb) as u32;
            (m as u64, false, sticky)
        } else {
            let lsb = lsb as u64;
            (
                self.bits_at(lsb),
                self.bit(lsb - 1),
                sticky || self.any_below(lsb - 1),
            )
        };
        let mut e = lsb + exp; // m · 2^e
        if round && (below || m & 1 == 1) {
            m += 1;
            if m == 1 << 53 {
                m >>= 1;
                e += 1;
            }
        }
        if m >> 52 == 0 {
            // Subnormal (e is −1074).
            return f64::from_bits(sign | m);
        }
        let field = e + 52 + 1023;
        if field >= 0x7ff {
            return f64::from_bits(sign | 0x7ff0_0000_0000_0000);
        }
        f64::from_bits(sign | ((field as u64) << 52) | (m & ((1 << 52) - 1)))
    }

    /// The low 128 bits of the magnitude (for values of at most 53 bits).
    fn mag_u128(&self) -> u128 {
        self.mag
            .iter()
            .take(4)
            .rev()
            .fold(0u128, |a, &d| (a << 32) | u128::from(d))
    }

    /// The decimal digits of the magnitude (`"0"` for zero).
    pub fn to_decimal(&self) -> String {
        if self.is_zero() {
            return "0".into();
        }
        let mut parts = Vec::new();
        let mut v = self.clone().abs();
        while !v.is_zero() {
            let (q, r) = v.divmod_mag(1_000_000_000);
            parts.push(r as u32);
            v = q;
        }
        let mut s = parts.last().unwrap().to_string();
        for p in parts.iter().rev().skip(1) {
            s.push_str(&format!("{p:09}"));
        }
        s
    }

    /// Compares two values.
    pub fn cmp_value(&self, other: &BigInt) -> Ordering {
        match (self.neg, other.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &other.mag),
            (true, true) => cmp_mag(&other.mag, &self.mag),
        }
    }
}

fn trim(m: &mut Vec<u32>) {
    while m.last() == Some(&0) {
        m.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in a.iter().enumerate() {
        let t = u64::from(x) + u64::from(*b.get(i).unwrap_or(&0)) + carry;
        out.push(t as u32);
        carry = t >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a − b`, `a ≥ b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut t = i64::from(x) - i64::from(*b.get(i).unwrap_or(&0)) - borrow;
        borrow = 0;
        if t < 0 {
            t += 1 << 32;
            borrow = 1;
        }
        out.push(t as u32);
    }
    trim(&mut out);
    out
}

/// A two's-complement integer of `N` 64-bit words, added to a row at a
/// time without allocating: the exact sums of integers and decimals and
/// of their squares that statistics keep per group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wide<const N: usize>(pub [u64; N]);

impl<const N: usize> Default for Wide<N> {
    fn default() -> Self {
        Wide([0; N])
    }
}

impl<const N: usize> Wide<N> {
    /// Adds an `i128`.
    #[inline]
    pub fn add_i128(&mut self, v: i128) {
        let ext = if v < 0 { u64::MAX } else { 0 };
        self.add_words(&[v as u64, (v >> 64) as u64], ext);
    }

    /// Adds a `u128` times 2^128 plus a `u128`: a 256-bit non-negative
    /// value (a decimal mantissa's square).
    #[inline]
    pub fn add_u256(&mut self, hi: u128, lo: u128) {
        self.add_words(
            &[lo as u64, (lo >> 64) as u64, hi as u64, (hi >> 64) as u64],
            0,
        );
    }

    #[inline]
    fn add_words(&mut self, w: &[u64], ext: u64) {
        let mut carry = false;
        for i in 0..N {
            let x = *w.get(i).unwrap_or(&ext);
            let (s1, c1) = self.0[i].overflowing_add(x);
            let (s2, c2) = s1.overflowing_add(u64::from(carry));
            self.0[i] = s2;
            carry = c1 || c2;
        }
    }

    /// Adds another.
    pub fn add(&mut self, o: &Wide<N>) {
        let w = o.0;
        self.add_words(&w, 0);
    }

    /// Whether the value's magnitude stays below 2^(64N − 2): sums that
    /// close to the edge are refused rather than wrapped.
    pub fn in_range(&self) -> bool {
        let top = self.0[N - 1] >> 61;
        top == 0 || top == 0b111
    }

    /// As a [`BigInt`].
    pub fn big(&self) -> BigInt {
        BigInt::from_words(&self.0)
    }

    /// Multiplied by 10^k (a decimal sum brought to a larger scale);
    /// `None` when it leaves the range.
    pub fn mul_pow10(&self, k: u32) -> Option<Self> {
        let b = self.big().mul_pow10(k);
        Self::from_big(&b)
    }

    /// From a [`BigInt`] that fits.
    pub fn from_big(b: &BigInt) -> Option<Self> {
        if b.bits() > 64 * N as u64 - 3 {
            return None;
        }
        let mut w = [0u64; N];
        for (i, d) in b.mag.iter().enumerate() {
            w[i / 2] |= u64::from(*d) << (32 * (i % 2));
        }
        let mut out = Wide(w);
        if b.neg {
            let mut carry = true;
            for x in &mut out.0 {
                let (y, c) = (!*x).overflowing_add(u64::from(carry));
                *x = y;
                carry = c;
            }
        }
        Some(out)
    }
}

/// `m²` for a decimal mantissa, as 256 bits `(hi, lo)`.
#[inline]
pub fn square_u256(m: i128) -> (u128, u128) {
    let a = m.unsigned_abs();
    let (a1, a0) = (a >> 64, a & u128::from(u64::MAX));
    let low = a0 * a0;
    let mid = a0 * a1; // twice, below
    let high = a1 * a1;
    let (lo, c1) = low.overflowing_add(mid << 65);
    let hi = high + (mid >> 63) + u128::from(c1);
    (hi, lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: i128) -> BigInt {
        BigInt::from_i128(v)
    }

    /// Long division against its own identity — a = q·d + r, 0 ≤ r < d —
    /// over random magnitudes, the cases that correct a digit's estimate
    /// (a divisor's top digit near the dividend's, the add-back) among
    /// them.
    #[test]
    fn long_division_holds() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3000 {
            let make = |next: &mut dyn FnMut() -> u64| {
                let len = next() % 9 + 1;
                let ws: Vec<u64> = (0..len)
                    .map(|_| match next() % 4 {
                        0 => 0,
                        1 => u64::MAX,
                        2 => 1 << 63,
                        _ => next(),
                    })
                    .collect();
                BigInt::from_words(&ws).abs()
            };
            let a = make(&mut next);
            let d = make(&mut next);
            if d.is_zero() {
                continue;
            }
            let (q, r) = a.div_big(&d);
            assert_eq!(q.mul(&d).add(&r).cmp_value(&a), Ordering::Equal);
            assert_eq!(r.cmp_value(&d), Ordering::Less);
            assert!(!r.is_negative());
        }
        // The add-back step: Knuth's own example shape.
        let a = BigInt::from_words(&[0, 0x8000_0000_0000_0000, 0x7fff_ffff_ffff_ffff]);
        let d = BigInt::from_words(&[1, 0x8000_0000_0000_0000, 0]);
        let (q, r) = a.div_big(&d);
        assert_eq!(q.mul(&d).add(&r).cmp_value(&a), Ordering::Equal);
    }

    #[test]
    fn arithmetic_matches_i128() {
        let vals = [
            0i128,
            1,
            -1,
            7,
            -12345678901234567,
            i64::MAX as i128,
            i64::MIN as i128,
            1 << 100,
            -(1 << 90) + 3,
        ];
        for &x in &vals {
            assert_eq!(b(x).to_i128(), Some(x));
            for &y in &vals {
                assert_eq!(b(x).add(&b(y)).to_i128(), x.checked_add(y), "{x} + {y}");
                assert_eq!(b(x).sub(&b(y)).to_i128(), x.checked_sub(y), "{x} - {y}");
                if let Some(p) = x.checked_mul(y) {
                    assert_eq!(b(x).mul(&b(y)).to_i128(), Some(p), "{x} * {y}");
                }
                assert_eq!(b(x).cmp_value(&b(y)), x.cmp(&y));
            }
        }
        assert_eq!(
            b(12345).mul_pow10(20).to_decimal(),
            "1234500000000000000000000"
        );
        assert_eq!(b(-5).shl(70).to_i128(), Some(-5 << 70));
        let (q, r) = b(1_000_000_007).mul(&b(1 << 80)).divmod_mag(998_244_353);
        let exact = 1_000_000_007i128 * (1 << 80);
        assert_eq!(q.to_i128(), Some(exact / 998_244_353));
        assert_eq!(r as i128, exact % 998_244_353);
        assert_eq!(b(7).div_round(2).to_i128(), Some(4));
        assert_eq!(b(-7).div_round(2).to_i128(), Some(-4));
        assert_eq!(b(-5).div_round(3).to_i128(), Some(-2));
        assert_eq!(b(10).div_round(4).to_i128(), Some(3));
        // A divisor past 96 bits goes the long way.
        let d = (1u128 << 100) + 12345;
        let n = b(987654321).mul(&BigInt::from_u128(d)).add(&b(55));
        let (q, r) = n.divmod_mag(d);
        assert_eq!((q.to_i128(), r), (Some(987654321), 55));
    }

    #[test]
    fn signed_digits_carry() {
        let cases: &[&[i64]] = &[
            &[],
            &[5],
            &[-5],
            &[0, 1],
            &[-1, 1],
            &[1, -1],
            &[i64::MAX, i64::MAX, -3],
            &[i64::MIN, 0, 0, 1],
            &[3, -(1 << 40), 7],
            &[-(1 << 32)],
            &[0, -(1 << 33)],
        ];
        for d in cases {
            let want = d.iter().enumerate().fold(BigInt::zero(), |a, (i, &x)| {
                a.add(&BigInt::from_i128(i128::from(x)).shl(32 * i as u64))
            });
            assert_eq!(BigInt::from_signed_digits(d), want, "{d:?}");
        }
    }

    #[test]
    fn square_roots_are_floors() {
        for v in [
            0u128,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            99,
            100,
            1 << 64,
            (1 << 64) + 1,
            u128::MAX,
        ] {
            let r = BigInt::from_u128(v).isqrt().to_i128().unwrap() as u128;
            assert!(
                r * r <= v && (r + 1).checked_mul(r + 1).is_none_or(|s| s > v),
                "{v}"
            );
        }
        let big = b(1 << 100).mul(&b(1 << 100)).add(&b(-1));
        assert_eq!(big.isqrt().add(&b(1)).sub(&b(1 << 100)).to_i128(), Some(0));
    }

    #[test]
    fn doubles_are_rounded_once() {
        assert_eq!(b(3).to_f64(0, false), 3.0);
        assert_eq!(b(-3).to_f64(-1, false), -1.5);
        assert_eq!(b(1).to_f64(-1074, false), 5e-324);
        assert_eq!(b(1).to_f64(-1075, false), 0.0);
        assert_eq!(b(1).to_f64(-1075, true), 5e-324);
        assert_eq!(b(3).to_f64(-1075, false), 1e-323);
        assert_eq!(b(1).to_f64(1024, false), f64::INFINITY);
        // 2^53 + 1: a tie, to even; with a sticky bit, up.
        let t = b((1 << 53) + 1);
        assert_eq!(t.to_f64(0, false), 9007199254740992.0);
        assert_eq!(t.to_f64(0, true), 9007199254740994.0);
        let mut seed = 0x1234_5678_9abc_def1u64;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let v = (seed >> (seed % 60)) as i64;
            let e = (seed % 200) as i64 - 100;
            let want = (v as f64) * 2f64.powi(e as i32);
            assert_eq!(b(i128::from(v)).to_f64(e, false), want, "{v} {e}");
        }
    }

    #[test]
    fn wide_sums_and_squares() {
        let mut w = Wide::<6>::default();
        w.add_i128(-5);
        w.add_i128(3);
        assert_eq!(w.big().to_i128(), Some(-2));
        let m = -(10i128.pow(38) - 1);
        let (hi, lo) = square_u256(m);
        let mut s = Wide::<6>::default();
        s.add_u256(hi, lo);
        assert_eq!(s.big(), b(m).mul(&b(m)));
        assert_eq!(Wide::<6>::from_big(&b(-77)).unwrap().big(), b(-77));
        assert_eq!(s.mul_pow10(2).unwrap().big(), b(m).mul(&b(m)).mul_pow10(2));
        assert!(s.in_range());
    }
}
