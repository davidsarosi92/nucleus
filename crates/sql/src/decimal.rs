//! `NUMERIC`: exact decimal numbers.
//!
//! A value is a 128-bit integer `m` and a scale `s` (digits after the
//! point): `m × 10^-s`.  Up to [`MAX_DIGITS`] significant digits; results
//! that need more are an error (`22003`), never silently rounded.  The scale
//! is kept as PostgreSQL keeps it (`1.50` stays `1.50`; `+` and `-` take the
//! larger scale, `*` the sum, `/` PostgreSQL's choice of at least 16
//! significant digits), and equal values of different scale compare and
//! hash equal.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

/// The most significant digits a value may have.
pub const MAX_DIGITS: u32 = 38;
const LIMIT: u128 = 10u128.pow(MAX_DIGITS) - 1;

/// An exact decimal number.
#[derive(Clone, Copy, Debug)]
pub struct Decimal {
    m: i128,
    scale: u8,
}

fn overflow() -> Error {
    Error::out_of_range("value overflows numeric format")
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

fn checked(m: i128, scale: u32) -> Result<Decimal> {
    if m.unsigned_abs() > LIMIT || scale > MAX_DIGITS {
        return Err(overflow());
    }
    Ok(Decimal {
        m,
        scale: scale as u8,
    })
}

/// `m / d` rounded half away from zero.
fn div_round(m: i128, d: i128) -> i128 {
    let (q, r) = (m / d, m % d);
    if r.unsigned_abs() * 2 >= d.unsigned_abs() {
        q + if (m < 0) == (d < 0) { 1 } else { -1 }
    } else {
        q
    }
}

impl Decimal {
    /// Zero.
    pub const ZERO: Decimal = Decimal { m: 0, scale: 0 };

    /// `m × 10^-scale`.
    pub fn new(m: i128, scale: u32) -> Result<Self> {
        checked(m, scale)
    }

    /// The unscaled integer.
    pub fn mantissa(self) -> i128 {
        self.m
    }

    /// Digits after the point.
    pub fn scale(self) -> u32 {
        u32::from(self.scale)
    }

    /// An integer.
    pub fn from_i64(i: i64) -> Self {
        Decimal {
            m: i128::from(i),
            scale: 0,
        }
    }

    /// Parses `[+-]digits[.digits][e[+-]digits]` (surrounding spaces
    /// allowed).
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || Error::invalid_text(format!("invalid input syntax for type numeric: \"{s}\""));
        let t = s.trim();
        let (neg, t) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let (num, exp) = match t.find(['e', 'E']) {
            Some(i) => (&t[..i], t[i + 1..].parse::<i32>().map_err(|_| bad())?),
            None => (t, 0),
        };
        let (int, frac) = match num.find('.') {
            Some(i) => (&num[..i], &num[i + 1..]),
            None => (num, ""),
        };
        if (int.is_empty() && frac.is_empty())
            || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let digits = format!("{int}{frac}");
        let digits = digits.trim_start_matches('0');
        // value = digits × 10^(exp - frac.len())
        let shift = i64::from(exp) - frac.len() as i64;
        let scale = (-shift).max(0);
        if digits.len() as i64 + shift.max(0) > i64::from(MAX_DIGITS)
            || scale > i64::from(MAX_DIGITS)
        {
            return Err(overflow());
        }
        let mut m: i128 = if digits.is_empty() {
            0
        } else {
            digits.parse().map_err(|_| overflow())?
        };
        if shift > 0 {
            m = m
                .checked_mul(pow10(shift as u32).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
        }
        checked(if neg { -m } else { m }, scale as u32)
    }

    /// The nearest decimal to a float, from its 15 significant digits (as
    /// PostgreSQL converts `float8` to `numeric`).
    pub fn from_f64(f: f64) -> Result<Self> {
        if !f.is_finite() {
            return Err(Error::feature("cannot convert NaN or infinity to numeric"));
        }
        let s = format!("{f:.14e}");
        let d = Self::parse(&s)?;
        Ok(d.trim())
    }

    /// A real's value to 6 significant digits (PostgreSQL's `float4` →
    /// `numeric`).
    pub fn from_f32(f: f32) -> Result<Self> {
        if !f.is_finite() {
            return Err(Error::feature("cannot convert NaN or infinity to numeric"));
        }
        Ok(Self::parse(&format!("{f:.5e}"))?.trim())
    }

    /// The nearest float.
    pub fn to_f64(self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// Rounded (half away from zero) to an integer.
    pub fn to_i64(self) -> Result<i64> {
        let r = self.round(0)?.m;
        i64::try_from(r).map_err(|_| Error::out_of_range("bigint out of range"))
    }

    /// Without trailing fractional zeros.
    pub fn trim(self) -> Self {
        let mut d = self;
        while d.scale > 0 && d.m % 10 == 0 {
            d.m /= 10;
            d.scale -= 1;
        }
        d
    }

    /// Rounded half away from zero to `digits` after the point (`digits`
    /// may be negative: tens, hundreds, …).  The result has scale
    /// `max(digits, 0)`.
    pub fn round(self, digits: i32) -> Result<Self> {
        self.rescale(digits, div_round)
    }

    /// Truncated toward zero to `digits` after the point.
    pub fn trunc(self, digits: i32) -> Result<Self> {
        self.rescale(digits, |m, d| m / d)
    }

    fn rescale(self, digits: i32, div: fn(i128, i128) -> i128) -> Result<Self> {
        let s = i64::from(self.scale);
        let target = i64::from(digits);
        if target >= s {
            let up = (target - s) as u32;
            let m = self
                .m
                .checked_mul(pow10(up).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
            return checked(m, target as u32);
        }
        let down = (s - target) as u32;
        let m = match pow10(down) {
            Some(p) => div(self.m, p),
            None => 0,
        };
        if target >= 0 {
            checked(m, target as u32)
        } else {
            let m = m
                .checked_mul(pow10((-target) as u32).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
            checked(m, 0)
        }
    }

    /// The largest integer not above this one.
    pub fn floor(self) -> Result<Self> {
        let t = self.trunc(0)?;
        if self.m < 0 && t.cmp(&self) != Ordering::Equal {
            checked(t.m - 1, 0)
        } else {
            Ok(t)
        }
    }

    /// The smallest integer not below this one.
    pub fn ceil(self) -> Result<Self> {
        let t = self.trunc(0)?;
        if self.m > 0 && t.cmp(&self) != Ordering::Equal {
            checked(t.m + 1, 0)
        } else {
            Ok(t)
        }
    }

    /// `-self`.
    pub fn negate(self) -> Self {
        Decimal {
            m: -self.m,
            scale: self.scale,
        }
    }

    /// `|self|`.
    pub fn abs(self) -> Self {
        Decimal {
            m: self.m.abs(),
            scale: self.scale,
        }
    }

    /// The sign: -1, 0 or 1.
    pub fn signum(self) -> i32 {
        self.m.signum() as i32
    }

    /// Both mantissas at the larger scale.
    fn aligned(self, other: Self) -> Result<(i128, i128, u32)> {
        let s = self.scale.max(other.scale);
        let up = |d: Decimal| {
            d.m.checked_mul(pow10(u32::from(s - d.scale)).ok_or_else(overflow)?)
                .ok_or_else(overflow)
        };
        Ok((up(self)?, up(other)?, u32::from(s)))
    }

    /// `self + other`.
    pub fn checked_add(self, other: Self) -> Result<Self> {
        let (a, b, s) = self.aligned(other)?;
        checked(a.checked_add(b).ok_or_else(overflow)?, s)
    }

    /// `self - other`.
    pub fn checked_sub(self, other: Self) -> Result<Self> {
        self.checked_add(other.negate())
    }

    /// `self × other`.
    pub fn checked_mul(self, other: Self) -> Result<Self> {
        let m = self.m.checked_mul(other.m).ok_or_else(overflow)?;
        checked(m, u32::from(self.scale) + u32::from(other.scale))
    }

    /// The position of the leading digit: `|v|` is in `[10^e, 10^(e+1))`.
    fn exponent(self) -> i64 {
        let digits = self.m.unsigned_abs().checked_ilog10().map_or(0, i64::from);
        digits - i64::from(self.scale)
    }

    /// PostgreSQL's base-10000 weight and leading base-10000 digit.
    fn nbase_lead(self) -> (i64, u128) {
        if self.m == 0 {
            return (0, 0);
        }
        let e = self.exponent();
        let weight = e.div_euclid(4);
        // The leading group: the digits from position e down to 4·weight.
        let mag = self.m.unsigned_abs();
        let low = 4 * weight + i64::from(self.scale); // digit index of 10^(4·weight) in mag
        let group = if low >= 0 {
            mag / 10u128.pow(low as u32)
        } else {
            mag * 10u128.pow((-low) as u32)
        };
        (weight, group % 10_000)
    }

    /// `self / other`, at PostgreSQL's result scale: at least 16
    /// significant digits, and no less than either operand's scale.
    pub fn checked_div(self, other: Self) -> Result<Self> {
        if other.m == 0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        let (w1, d1) = self.nbase_lead();
        let (w2, d2) = other.nbase_lead();
        let mut qweight = w1 - w2;
        if d1 <= d2 {
            qweight -= 1;
        }
        let rscale = (16 - qweight * 4)
            .max(i64::from(self.scale))
            .max(i64::from(other.scale))
            .clamp(0, i64::from(MAX_DIGITS));
        self.div_at(other, rscale as u32)
    }

    /// `self / other` rounded to `rscale` digits after the point.
    pub fn div_at(self, other: Self, rscale: u32) -> Result<Self> {
        if other.m == 0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        // q = m1 / m2 × 10^(rscale + s2 - s1), by long division.
        let k = i64::from(rscale) + i64::from(other.scale) - i64::from(self.scale);
        let (mut a, b) = (self.m.unsigned_abs(), other.m.unsigned_abs());
        let mut b = b;
        if k < 0 {
            b = b
                .checked_mul(10u128.checked_pow((-k) as u32).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
        }
        let mut q = a / b;
        a %= b;
        let mut extra = k.max(0);
        let round_up = loop {
            // One more digit than asked, to round on.
            let r10 = a.checked_mul(10).ok_or_else(overflow)?;
            let digit = r10 / b;
            a = r10 % b;
            if extra == 0 {
                break digit >= 5;
            }
            q = q
                .checked_mul(10)
                .and_then(|q| q.checked_add(digit))
                .filter(|q| *q <= LIMIT)
                .ok_or_else(overflow)?;
            extra -= 1;
        };
        if round_up {
            q += 1;
        }
        if q > LIMIT {
            return Err(overflow());
        }
        let m = q as i128;
        checked(if (self.m < 0) != (other.m < 0) { -m } else { m }, rscale)
    }

    /// `self % other` (sign of `self`, as PostgreSQL).
    pub fn checked_rem(self, other: Self) -> Result<Self> {
        if other.m == 0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        let (a, b, s) = self.aligned(other)?;
        checked(a % b, s)
    }

    /// Enforces `NUMERIC(precision, scale)`: rounds to `scale` digits, and
    /// fails if more than `precision - scale` digits remain before the
    /// point.
    pub fn fit(self, precision: u32, scale: u32) -> Result<Self> {
        let r = self.round(scale as i32)?;
        let int_digits = if r.m == 0 {
            0
        } else {
            (r.exponent() + 1).max(0)
        };
        if int_digits > i64::from(precision) - i64::from(scale) {
            return Err(Error::out_of_range(format!(
                "numeric field overflow: a field with precision {precision}, scale {scale} must round to an absolute value less than 10^{}",
                precision - scale
            )));
        }
        Ok(r)
    }

    /// The digits (without trailing zeros) and the leading digit's
    /// position, for order-preserving keys; `None` for zero.
    pub fn digits(self) -> Option<(Vec<u8>, i64)> {
        if self.m == 0 {
            return None;
        }
        let t = self.trim();
        let s = t.m.unsigned_abs().to_string();
        let digits: Vec<u8> = s.trim_end_matches('0').bytes().map(|b| b - b'0').collect();
        Some((digits, t.exponent()))
    }

    /// The value with digits `d1 d2 … dn` (no trailing zeros), leading
    /// digit at 10^`exp`, and the given sign.
    pub fn from_digits(negative: bool, digits: &[u8], exp: i64) -> Result<Self> {
        let mut m: i128 = 0;
        for &d in digits {
            m = m
                .checked_mul(10)
                .and_then(|m| m.checked_add(i128::from(d)))
                .ok_or_else(overflow)?;
        }
        // value = m × 10^(exp - (n - 1))
        let shift = exp - (digits.len() as i64 - 1);
        let (m, scale) = if shift >= 0 {
            (
                m.checked_mul(pow10(shift as u32).ok_or_else(overflow)?)
                    .ok_or_else(overflow)?,
                0,
            )
        } else {
            (m, (-shift) as u32)
        };
        checked(if negative { -m } else { m }, scale)
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.aligned(*other) {
            Ok((a, b, _)) => a.cmp(&b),
            // Too far apart in scale to align: compare by sign, then by
            // magnitude through the digits.
            Err(_) => {
                let sign = self.signum().cmp(&other.signum());
                if sign != Ordering::Equal || self.m == 0 {
                    return sign;
                }
                let (da, ea) = self.digits().expect("non-zero");
                let (db, eb) = other.digits().expect("non-zero");
                let mag = ea.cmp(&eb).then_with(|| da.cmp(&db));
                if self.m < 0 { mag.reverse() } else { mag }
            }
        }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.m.unsigned_abs().to_string();
        let s = usize::from(self.scale);
        let sign = if self.m < 0 { "-" } else { "" };
        if s == 0 {
            return write!(f, "{sign}{digits}");
        }
        let padded = format!("{digits:0>width$}", width = s + 1);
        let (int, frac) = padded.split_at(padded.len() - s);
        write!(f, "{sign}{int}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    #[test]
    fn parse_and_print() {
        for (input, out) in [
            ("1.50", "1.50"),
            ("-0.05", "-0.05"),
            (".5", "0.5"),
            ("5.", "5"),
            ("007", "7"),
            ("1.5e3", "1500"),
            ("1.5e-3", "0.0015"),
            ("-12e2", "-1200"),
            ("0", "0"),
            ("0.000", "0.000"),
            (" 42 ", "42"),
        ] {
            assert_eq!(d(input).to_string(), out, "{input}");
        }
        for bad in ["", ".", "1.2.3", "abc", "1e", "--1"] {
            assert!(Decimal::parse(bad).is_err(), "{bad}");
        }
        assert!(Decimal::parse(&"9".repeat(39)).is_err());
        assert!(Decimal::parse(&"9".repeat(38)).is_ok());
    }

    #[test]
    fn arithmetic_matches_postgresql() {
        assert_eq!(
            d("1.50").checked_add(d("2.255")).unwrap().to_string(),
            "3.755"
        );
        assert_eq!(d("1.5").checked_sub(d("2")).unwrap().to_string(), "-0.5");
        assert_eq!(
            d("1.50").checked_mul(d("2.0")).unwrap().to_string(),
            "3.000"
        );
        // PostgreSQL's division scales.
        for (a, b, q) in [
            ("1", "3", "0.33333333333333333333"),
            ("7", "2.0", "3.5000000000000000"),
            ("2", "3", "0.66666666666666666667"),
            ("10", "4", "2.5000000000000000"),
            ("100", "7", "14.2857142857142857"),
            ("1", "8", "0.12500000000000000000"),
            ("12345.678", "0.001", "12345678.000000000000"),
            ("-1", "3", "-0.33333333333333333333"),
            ("0", "5", "0.00000000000000000000"),
        ] {
            assert_eq!(d(a).checked_div(d(b)).unwrap().to_string(), q, "{a} / {b}");
        }
        assert_eq!(d("7.5").checked_rem(d("2")).unwrap().to_string(), "1.5");
        assert_eq!(d("-7.5").checked_rem(d("2")).unwrap().to_string(), "-1.5");
        assert_eq!(d("1").checked_div(d("0")).unwrap_err().code, "22012");
        let big = d(&"9".repeat(38));
        assert_eq!(big.checked_add(d("1")).unwrap_err().code, "22003");
    }

    #[test]
    fn rounding_and_typmods() {
        assert_eq!(d("2.5").round(0).unwrap().to_string(), "3");
        assert_eq!(d("-2.5").round(0).unwrap().to_string(), "-3");
        assert_eq!(d("1.2345").round(2).unwrap().to_string(), "1.23");
        assert_eq!(d("1.235").round(2).unwrap().to_string(), "1.24");
        assert_eq!(d("1234.5").round(-2).unwrap().to_string(), "1200");
        assert_eq!(d("1.2").round(3).unwrap().to_string(), "1.200");
        assert_eq!(d("-1.7").trunc(0).unwrap().to_string(), "-1");
        assert_eq!(d("-1.2").floor().unwrap().to_string(), "-2");
        assert_eq!(d("1.2").ceil().unwrap().to_string(), "2");
        assert_eq!(d("123.456").fit(5, 2).unwrap().to_string(), "123.46");
        assert_eq!(d("999.995").fit(5, 2).unwrap_err().code, "22003");
        assert_eq!(d("0.5").to_i64().unwrap(), 1);
        assert_eq!(Decimal::from_f64(0.1).unwrap().to_string(), "0.1");
        assert_eq!(Decimal::from_f64(0.1 + 0.2).unwrap().to_string(), "0.3");
        assert_eq!(
            Decimal::from_f64(1e20).unwrap().to_string(),
            "100000000000000000000"
        );
        assert_eq!(d("0.1").to_f64(), 0.1);
    }

    #[test]
    fn order_and_digits() {
        let vals = [
            "-100", "-1.5", "-1.25", "-1.2", "0", "0.001", "1", "1.00", "1.2", "1.25", "10",
        ];
        for w in vals.windows(2) {
            assert!(d(w[0]) <= d(w[1]), "{} <= {}", w[0], w[1]);
        }
        assert_eq!(d("1"), d("1.000"));
        let (digits, e) = d("120.50").digits().unwrap();
        assert_eq!((digits, e), (vec![1, 2, 0, 5], 2));
        assert_eq!(
            Decimal::from_digits(true, &[1, 2, 0, 5], 2)
                .unwrap()
                .to_string(),
            "-120.5"
        );
        assert_eq!(
            Decimal::from_digits(false, &[5], -3).unwrap().to_string(),
            "0.005"
        );
        assert_eq!(
            Decimal::from_digits(false, &[5], 3).unwrap().to_string(),
            "5000"
        );
    }
}
