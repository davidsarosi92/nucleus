//! Column segments' encodings (PLAN.md §M14.20 V7): one column of a
//! segment's rows as one value, encoded by what its values are —
//!
//! - integers, dates, times and timestamps: frame of reference and bit
//!   packing (each value less the least, in as many bits as the largest
//!   difference needs), or runs of equal values when there are few;
//! - doubles as they are, booleans a bit each;
//! - text (and `char(n)`, as its own kinds): a dictionary and bit-packed
//!   codes when values repeat, else offsets and bytes (Arrow's layout);
//! - decimals of one scale whose unscaled values fit 64 bits: those
//!   integers, by frame of reference and bit packing;
//! - anything else: each value's row encoding, back to back.
//!
//! `NULL`s are a bitmap beside the values.  Decoding gives the vectorized
//! executor's vectors directly ([`Vector`]), typed as a scan of the row
//! storage gives them; [`value_at`] reads one row's value from the bytes
//! where the layout allows (a point read: no column decoded for it).

use crate::encoding::{Reader, decode_value, encode_value};
use crate::error::{Error, Result};
use crate::types::Value;
use crate::vector::{TimeKind, Vector};

const VALUES: u8 = 0;
const INTS: u8 = 1;
const RUNS: u8 = 2;
const FLOATS: u8 = 3;
const BOOLS: u8 = 4;
const DICT: u8 = 5;
const TEXT: u8 = 6;
const DECIMALS: u8 = 7;
/// `DICT` and `TEXT` of `char(n)` values.
const CHAR_DICT: u8 = 8;
const CHAR_TEXT: u8 = 9;

fn bad() -> Error {
    Error::internal("a malformed column segment")
}

fn time_kind(kind: u8) -> Option<TimeKind> {
    Some(match kind {
        1 => TimeKind::Date,
        2 => TimeKind::Time,
        3 => TimeKind::Timestamp,
        4 => TimeKind::TimestampTz,
        _ => return None,
    })
}

fn bits_for(x: u64) -> u8 {
    (64 - x.leading_zeros()) as u8
}

/// `values` in `bits` bits each, little-endian.
fn pack(values: impl Iterator<Item = u64>, bits: u8, out: &mut Vec<u8>) {
    if bits == 0 {
        return;
    }
    let (mut acc, mut have) = (0u128, 0u32);
    for v in values {
        acc |= u128::from(v) << have;
        have += u32::from(bits);
        while have >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            have -= 8;
        }
    }
    if have > 0 {
        out.push(acc as u8);
    }
}

fn unpack(b: &[u8], n: usize, bits: u8) -> Result<Vec<u64>> {
    if bits == 0 {
        return Ok(vec![0; n]);
    }
    if bits > 64 || b.len() < (n * bits as usize).div_ceil(8) {
        return Err(bad());
    }
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut have, mut i) = (0u128, 0u32, 0);
    for _ in 0..n {
        while have < u32::from(bits) {
            acc |= u128::from(b[i]) << have;
            i += 1;
            have += 8;
        }
        out.push(acc as u64 & mask);
        acc >>= bits;
        have -= u32::from(bits);
    }
    Ok(out)
}

fn bitmap(bits: impl Iterator<Item = bool>, out: &mut Vec<u8>) {
    pack(bits.map(u64::from), 1, out);
}

/// A column's statistics: its least and greatest values not `NULL` (`NULL`
/// for none), in SQL's order, and how many are `NULL`.
pub type Stats = (Value, Value, u64);

/// One column of a segment — its values taken — encoded, with its
/// statistics.  The values make a typed vector first ([`Vector::typed`],
/// the one builder of a column from values: PLAN.md §M14.20 W3b), which
/// is encoded and measured from its arrays — the integers' least and
/// greatest by the kernels; values of no one type are encoded each as it
/// is ([`stats`] measures them).
pub fn encode(values: Vec<Value>) -> (Vec<u8>, Stats) {
    let n = values.len();
    match Vector::typed(values) {
        Vector::Values(values) => (encode_values(&values), stats(&values)),
        v => encode_vector(v, n),
    }
}

/// The header of a column of `n` values of `kind`, and its `NULL`s'
/// bitmap when there are any.
fn header(out: &mut Vec<u8>, kind: u8, n: usize, nulls: Option<&[bool]>) {
    out.push(kind);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    match nulls.filter(|n| n.contains(&true)) {
        Some(nulls) => {
            out.push(1);
            bitmap(nulls.iter().copied(), out);
        }
        None => out.push(0),
    }
}

/// Values of no one type: each as its row encoding (none but `NULL`s: any
/// typed kind will do).
fn encode_values(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    let nulls: Vec<bool> = values.iter().map(Value::is_null).collect();
    if !nulls.contains(&false) {
        header(&mut out, BOOLS, values.len(), Some(&nulls));
        bitmap(values.iter().map(|_| false), &mut out);
        return out;
    }
    header(&mut out, VALUES, values.len(), Some(&nulls));
    for v in values {
        encode_value(&mut out, v);
    }
    out
}

/// The rows of the least and greatest values not `NULL` by `cmp` (the
/// first of equals), if any.
fn extremes(
    n: usize,
    nulls: Option<&[bool]>,
    cmp: impl Fn(usize, usize) -> std::cmp::Ordering,
) -> Option<(usize, usize)> {
    let mut out: Option<(usize, usize)> = None;
    for i in (0..n).filter(|&i| !nulls.is_some_and(|n| n[i])) {
        out = Some(match out {
            None => (i, i),
            Some((lo, hi)) => (
                if cmp(i, lo).is_lt() { i } else { lo },
                if cmp(i, hi).is_gt() { i } else { hi },
            ),
        });
    }
    out
}

/// A typed column of `n` rows (never [`Vector::Values`]), encoded and
/// measured.
fn encode_vector(v: Vector, n: usize) -> (Vec<u8>, Stats) {
    let mut out = Vec::new();
    let nulls_of = |nulls: &Option<Vec<bool>>| {
        nulls
            .as_deref()
            .map_or(0, |n| n.iter().filter(|&&b| b).count() as u64)
    };
    // The statistics of the rows of the extremes `cmp` finds.
    let measured = |v: &Vector,
                    nulls: &Option<Vec<bool>>,
                    cmp: &dyn Fn(usize, usize) -> std::cmp::Ordering|
     -> Stats {
        match extremes(n, nulls.as_deref(), cmp) {
            Some((lo, hi)) => (v.value(lo), v.value(hi), nulls_of(nulls)),
            None => (Value::Null, Value::Null, nulls_of(nulls)),
        }
    };
    match &v {
        Vector::Int(d, nulls) | Vector::Time(_, d, nulls) => {
            let (k, value): (u8, &dyn Fn(i64) -> Value) = match &v {
                Vector::Time(kind, ..) => (
                    match kind {
                        TimeKind::Date => 1,
                        TimeKind::Time => 2,
                        TimeKind::Timestamp => 3,
                        TimeKind::TimestampTz => 4,
                    },
                    &|x| kind.value(x),
                ),
                _ => (0, &Value::Int),
            };
            encode_ints(&mut out, k, d, nulls.as_deref());
            let at = |max| {
                crate::kernels::minmax_i64(d, nulls.as_deref(), max).map_or(Value::Null, value)
            };
            let stats = (at(false), at(true), nulls_of(nulls));
            (out, stats)
        }
        Vector::Float(d, nulls) => {
            header(&mut out, FLOATS, n, nulls.as_deref());
            for f in d {
                out.extend_from_slice(&f.to_le_bytes());
            }
            let stats = measured(&v, nulls, &|i, j| crate::types::total_cmp(d[i], d[j]));
            (out, stats)
        }
        Vector::Bool(d, nulls) => {
            header(&mut out, BOOLS, n, nulls.as_deref());
            bitmap(d.iter().copied(), &mut out);
            let stats = measured(&v, nulls, &|i, j| d[i].cmp(&d[j]));
            (out, stats)
        }
        Vector::Text(o, b, nulls) | Vector::Char(o, b, nulls) => {
            let char = matches!(v, Vector::Char(..));
            let at = |i: usize| &b[o[i] as usize..o[i + 1] as usize];
            encode_texts(&mut out, char, n, o, b, nulls.as_deref());
            // char(n) compares without its trailing blanks.
            let key = |i: usize| {
                let s = at(i);
                if char {
                    &s[..s.len() - s.iter().rev().take_while(|&&c| c == b' ').count()]
                } else {
                    s
                }
            };
            let stats = measured(&v, nulls, &|i, j| key(i).cmp(key(j)));
            (out, stats)
        }
        Vector::Dec(scale, ms, nulls) if (0..ms.len()).all(|i| i64::try_from(ms.at(i)).is_ok()) => {
            header(&mut out, DECIMALS, n, nulls.as_deref());
            let at = |i: usize| ms.at(i) as i64;
            let lo = (0..ms.len()).map(at).min().unwrap_or(0);
            let hi = (0..ms.len()).map(at).max().unwrap_or(0);
            let bits = bits_for((hi as i128 - lo as i128) as u64);
            out.push(*scale as u8);
            out.extend_from_slice(&lo.to_le_bytes());
            out.push(bits);
            pack(
                (0..ms.len()).map(|i| at(i).wrapping_sub(lo) as u64),
                bits,
                &mut out,
            );
            let stats = measured(&v, nulls, &|i, j| ms.at(i).cmp(&ms.at(j)));
            (out, stats)
        }
        _ => {
            // Decimals past 64 bits (or a kind of no encoding of its own):
            // each value as it is.
            let values: Vec<Value> = (0..n).map(|i| v.value(i)).collect();
            (encode_values(&values), stats(&values))
        }
    }
}

/// Integers of kind `k` (0, or a time kind's), a `NULL` as 0: runs of
/// equal values when there are few, else frame of reference and bit
/// packing.
fn encode_ints(out: &mut Vec<u8>, k: u8, ints: &[i64], nulls: Option<&[bool]>) {
    let n = ints.len();
    let runs = 1 + ints.windows(2).filter(|w| w[0] != w[1]).count();
    let lo = ints.iter().copied().min().unwrap_or(0);
    let hi = ints.iter().copied().max().unwrap_or(0);
    let bits = bits_for((hi as i128 - lo as i128) as u64);
    if runs * 12 < n * bits as usize / 8 {
        header(out, RUNS, n, nulls);
        out.push(k);
        out.extend_from_slice(&(runs as u32).to_le_bytes());
        let mut i = 0;
        while i < n {
            let mut j = i;
            while j < n && ints[j] == ints[i] {
                j += 1;
            }
            out.extend_from_slice(&ints[i].to_le_bytes());
            out.extend_from_slice(&((j - i) as u32).to_le_bytes());
            i = j;
        }
    } else {
        header(out, INTS, n, nulls);
        out.push(k);
        out.extend_from_slice(&lo.to_le_bytes());
        out.push(bits);
        pack(ints.iter().map(|&x| x.wrapping_sub(lo) as u64), bits, out);
    }
}

/// Strings laid out as a text vector's (a `NULL` empty): a sorted
/// dictionary and bit-packed codes when at most a quarter of them are
/// distinct, else offsets and bytes as they are.  The distinct ones are
/// counted by hashing, stopping past the quarter — no sort of every
/// string, only of a dictionary's.
fn encode_texts(
    out: &mut Vec<u8>,
    char: bool,
    n: usize,
    offsets: &[u32],
    bytes: &[u8],
    nulls: Option<&[bool]>,
) {
    let (dict_kind, text_kind) = if char {
        (CHAR_DICT, CHAR_TEXT)
    } else {
        (DICT, TEXT)
    };
    let at = |i: usize| &bytes[offsets[i] as usize..offsets[i + 1] as usize];
    let live = || (0..n).filter(|&i| !nulls.is_some_and(|n| n[i]));
    let room = (n / 4).min((1 << 20) - 1);
    let mut distinct: std::collections::HashSet<&[u8], crate::fasthash::FastBuild> =
        std::collections::HashSet::default();
    let few = live().all(|i| {
        distinct.insert(at(i));
        distinct.len() <= room
    });
    if few {
        let mut dict: Vec<&[u8]> = distinct.into_iter().collect();
        dict.sort_unstable();
        header(out, dict_kind, n, nulls);
        out.extend_from_slice(&(dict.len() as u32).to_le_bytes());
        for d in &dict {
            out.extend_from_slice(&(d.len() as u32).to_le_bytes());
            out.extend_from_slice(d);
        }
        let bits = bits_for(dict.len().saturating_sub(1) as u64);
        out.push(bits);
        let code = |i: usize| {
            if nulls.is_some_and(|n| n[i]) {
                0
            } else {
                dict.binary_search(&at(i)).expect("in the dictionary") as u64
            }
        };
        pack((0..n).map(code), bits, out);
    } else {
        header(out, text_kind, n, nulls);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        for o in offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(bytes);
    }
}

/// The `bits`-bit value at place `i` of a packed run.
fn packed_at(b: &[u8], i: usize, bits: u8) -> Result<u64> {
    if bits == 0 {
        return Ok(0);
    }
    if bits > 64 {
        return Err(bad());
    }
    let start = i * bits as usize;
    let (byte, shift) = (start / 8, start % 8);
    let need = (shift + bits as usize).div_ceil(8);
    let src = b.get(byte..byte + need).ok_or_else(bad)?;
    let mut acc = 0u128;
    for (k, &x) in src.iter().enumerate() {
        acc |= u128::from(x) << (8 * k);
    }
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    Ok((acc >> shift) as u64 & mask)
}

/// Row `i`'s value of a column [`encode`]d, read where it lies — `None`
/// for a layout that must be decoded whole to reach it (a dictionary, row
/// encodings back to back).
pub fn value_at(b: &[u8], i: usize) -> Result<Option<Value>> {
    let mut r = Reader::new(b);
    let kind = r.u8().map_err(|_| bad())?;
    let n = r.u32().map_err(|_| bad())? as usize;
    if i >= n {
        return Err(bad());
    }
    if r.u8().map_err(|_| bad())? != 0 {
        let bytes = r.take(n.div_ceil(8)).map_err(|_| bad())?;
        if packed_at(bytes, i, 1)? == 1 {
            return Ok(Some(Value::Null));
        }
    }
    let int = |k: u8, x: i64| match time_kind(k) {
        None if k == 0 => Ok(Value::Int(x)),
        Some(t) => Ok(t.value(x)),
        None => Err(bad()),
    };
    Ok(Some(match kind {
        INTS => {
            let k = r.u8().map_err(|_| bad())?;
            let lo = r.u64().map_err(|_| bad())? as i64;
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            int(k, lo.wrapping_add(packed_at(rest, i, bits)? as i64))?
        }
        RUNS => {
            let k = r.u8().map_err(|_| bad())?;
            let runs = r.u32().map_err(|_| bad())? as usize;
            let mut at = 0;
            let mut found = None;
            for _ in 0..runs {
                let x = r.u64().map_err(|_| bad())? as i64;
                at += r.u32().map_err(|_| bad())? as usize;
                if i < at {
                    found = Some(x);
                    break;
                }
            }
            int(k, found.ok_or_else(bad)?)?
        }
        FLOATS => {
            r.take(8 * i).map_err(|_| bad())?;
            Value::Float(f64::from_bits(r.u64().map_err(|_| bad())?))
        }
        BOOLS => {
            let bytes = r.take(n.div_ceil(8)).map_err(|_| bad())?;
            Value::Bool(packed_at(bytes, i, 1)? == 1)
        }
        TEXT | CHAR_TEXT => {
            let total = r.u32().map_err(|_| bad())? as usize;
            let offsets = r.take(4 * (n + 1)).map_err(|_| bad())?;
            let o = |k: usize| {
                u32::from_le_bytes(offsets[4 * k..4 * k + 4].try_into().expect("4")) as usize
            };
            let (from, to) = (o(i), o(i + 1));
            let bytes = r.take(total).map_err(|_| bad())?;
            let s = bytes.get(from..to).ok_or_else(bad)?;
            text_value(kind, s)?
        }
        DICT | CHAR_DICT => {
            // Past the dictionary (lengths to skip by) to the row's code.
            let m = r.u32().map_err(|_| bad())? as usize;
            let mut entries = Vec::with_capacity(m.min(n));
            for _ in 0..m {
                let len = r.u32().map_err(|_| bad())? as usize;
                entries.push(r.take(len).map_err(|_| bad())?);
            }
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            let s = entries
                .get(packed_at(rest, i, bits)? as usize)
                .ok_or_else(bad)?;
            text_value(kind, s)?
        }
        DECIMALS => {
            let scale = r.u8().map_err(|_| bad())?;
            let lo = r.u64().map_err(|_| bad())? as i64;
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            let m = lo.wrapping_add(packed_at(rest, i, bits)? as i64);
            Value::Numeric(crate::decimal::Decimal::new(
                i128::from(m),
                u32::from(scale),
            )?)
        }
        _ => return Ok(None),
    }))
}

/// A text kind's value: `text`, or `char(n)` for the `CHAR_` kinds.
fn text_value(kind: u8, s: &[u8]) -> Result<Value> {
    let s = std::str::from_utf8(s).map_err(|_| bad())?.to_string();
    Ok(if matches!(kind, CHAR_DICT | CHAR_TEXT) {
        Value::Char(s)
    } else {
        Value::Text(s)
    })
}

/// `char(n)` values decoded as text: a char vector of the same bytes.
fn chars(v: Vector) -> Vector {
    match v {
        Vector::Text(offsets, bytes, nulls) => Vector::Char(offsets, bytes, nulls),
        v => v,
    }
}

/// A column [`encode`]d, as a vector.
pub fn decode(b: &[u8]) -> Result<Vector> {
    let kind = *b.first().ok_or_else(bad)?;
    let v = decode_kinds(b)?;
    Ok(if matches!(kind, CHAR_DICT | CHAR_TEXT) {
        chars(v)
    } else {
        v
    })
}

fn decode_kinds(b: &[u8]) -> Result<Vector> {
    let mut r = Reader::new(b);
    let kind = r.u8().map_err(|_| bad())?;
    let n = r.u32().map_err(|_| bad())? as usize;
    let nulls = match r.u8().map_err(|_| bad())? {
        0 => None,
        _ => {
            let bytes = r.take(n.div_ceil(8)).map_err(|_| bad())?;
            Some(
                unpack(bytes, n, 1)?
                    .into_iter()
                    .map(|x| x == 1)
                    .collect::<Vec<bool>>(),
            )
        }
    };
    let ints = |k: u8, data: Vec<i64>, nulls| match time_kind(k) {
        None if k == 0 => Ok(Vector::Int(data, nulls)),
        Some(t) => Ok(Vector::Time(t, data, nulls)),
        None => Err(bad()),
    };
    match kind {
        INTS => {
            let k = r.u8().map_err(|_| bad())?;
            let lo = r.u64().map_err(|_| bad())? as i64;
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            let data = unpack(rest, n, bits)?
                .into_iter()
                .map(|d| lo.wrapping_add(d as i64))
                .collect();
            ints(k, data, nulls)
        }
        RUNS => {
            let k = r.u8().map_err(|_| bad())?;
            let runs = r.u32().map_err(|_| bad())? as usize;
            let mut data = Vec::with_capacity(n);
            for _ in 0..runs {
                let x = r.u64().map_err(|_| bad())? as i64;
                let c = r.u32().map_err(|_| bad())? as usize;
                if data.len() + c > n {
                    return Err(bad());
                }
                data.resize(data.len() + c, x);
            }
            if data.len() != n {
                return Err(bad());
            }
            ints(k, data, nulls)
        }
        FLOATS => {
            let mut data = Vec::with_capacity(n);
            for _ in 0..n {
                data.push(f64::from_bits(r.u64().map_err(|_| bad())?));
            }
            Ok(Vector::Float(data, nulls))
        }
        BOOLS => {
            let bytes = r.take(n.div_ceil(8)).map_err(|_| bad())?;
            let data = unpack(bytes, n, 1)?.into_iter().map(|x| x == 1).collect();
            Ok(Vector::Bool(data, nulls))
        }
        DICT | CHAR_DICT => {
            let m = r.u32().map_err(|_| bad())? as usize;
            let mut dict = Vec::with_capacity(m.min(n));
            for _ in 0..m {
                let len = r.u32().map_err(|_| bad())? as usize;
                dict.push(r.take(len).map_err(|_| bad())?);
            }
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            // As stored: the entries, and each row's code (a `NULL`'s 0).
            let mut codes = Vec::with_capacity(n);
            for (i, c) in unpack(rest, n, bits)?.into_iter().enumerate() {
                let null = nulls.as_ref().is_some_and(|z| z[i]);
                if !null && c as usize >= dict.len() {
                    return Err(bad());
                }
                codes.push(if null { 0 } else { c as u32 });
            }
            let mut offsets = Vec::with_capacity(dict.len() + 1);
            let mut bytes = Vec::new();
            offsets.push(0u32);
            for d in &dict {
                check_utf8(d)?;
                bytes.extend_from_slice(d);
                offsets.push(bytes.len() as u32);
            }
            let entries = if kind == CHAR_DICT {
                Vector::Char(offsets, bytes, None)
            } else {
                Vector::Text(offsets, bytes, None)
            };
            Ok(Vector::Dict(std::sync::Arc::new(entries), codes, nulls))
        }
        TEXT | CHAR_TEXT => {
            let total = r.u32().map_err(|_| bad())? as usize;
            let mut offsets = Vec::with_capacity(n + 1);
            for _ in 0..=n {
                let o = r.u32().map_err(|_| bad())?;
                if o as usize > total || offsets.last().is_some_and(|&p| o < p) {
                    return Err(bad());
                }
                offsets.push(o);
            }
            let bytes = r.take(total).map_err(|_| bad())?.to_vec();
            check_utf8(&bytes)?;
            Ok(Vector::Text(offsets, bytes, nulls))
        }
        VALUES => {
            let mut vals = Vec::with_capacity(n);
            for _ in 0..n {
                vals.push(decode_value(&mut r)?);
            }
            Ok(Vector::Values(vals))
        }
        DECIMALS => {
            let scale = u32::from(r.u8().map_err(|_| bad())?);
            let lo = r.u64().map_err(|_| bad())? as i64;
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            if scale > crate::decimal::MAX_DIGITS {
                return Err(bad());
            }
            // As stored: one scale, each row's mantissa (a `NULL`'s 0; an
            // `i64`'s within the digits a decimal may have).
            let ms = unpack(rest, n, bits)?;
            let data = ms
                .into_iter()
                .enumerate()
                .map(|(i, d)| {
                    if nulls.as_ref().is_some_and(|z| z[i]) {
                        0
                    } else {
                        lo.wrapping_add(d as i64)
                    }
                })
                .collect();
            Ok(Vector::Dec(
                scale,
                crate::vector::Mantissas::Narrow(data),
                nulls,
            ))
        }
        _ => Err(bad()),
    }
}

fn check_utf8(b: &[u8]) -> Result<()> {
    std::str::from_utf8(b).map(|_| ()).map_err(|_| bad())
}

/// A column's least and greatest values not `NULL` (`NULL` for none), in
/// SQL's order, and how many are `NULL`.
pub fn stats(values: &[Value]) -> (Value, Value, u64) {
    let mut lo: Option<&Value> = None;
    let mut hi: Option<&Value> = None;
    let mut nulls = 0;
    for v in values {
        if v.is_null() {
            nulls += 1;
            continue;
        }
        if lo.is_none_or(|l| v.sort_cmp(l).is_lt()) {
            lo = Some(v);
        }
        if hi.is_none_or(|h| v.sort_cmp(h).is_gt()) {
            hi = Some(v);
        }
    }
    (
        lo.cloned().unwrap_or(Value::Null),
        hi.cloned().unwrap_or(Value::Null),
        nulls,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decimal's unscaled value and scale, when the unscaled value fits 64
    /// bits.
    fn dec_of(v: &Value) -> Option<(i64, u32)> {
        match v {
            Value::Numeric(d) => Some((i64::try_from(d.mantissa()).ok()?, d.scale())),
            _ => None,
        }
    }

    /// An integer-like value: its kind (0 integer, else a [`TimeKind`]) and
    /// integer.
    fn int_of(v: &Value) -> Option<(u8, i64)> {
        Some(match v {
            Value::Int(i) => (0, *i),
            Value::Date(d) => (1, i64::from(*d)),
            Value::Time(t) => (2, *t),
            Value::Timestamp(t) => (3, *t),
            Value::TimestampTz(t) => (4, *t),
            _ => return None,
        })
    }

    /// The encoder as it was before typed vectors (PLAN.md §M14.20 W3b),
    /// value by value: what [`encode`] must write byte for byte.
    fn reference(values: &[Value]) -> Vec<u8> {
        let n = values.len();
        let mut out = Vec::new();
        let live = || values.iter().filter(|v| !v.is_null());
        let first = live().next();
        let kind = match first {
            None => BOOLS, // all NULL: any typed kind will do
            Some(f) if int_of(f).is_some() => {
                let k = int_of(f).expect("an integer").0;
                if live().all(|v| int_of(v).is_some_and(|(kv, _)| kv == k)) {
                    INTS
                } else {
                    VALUES
                }
            }
            Some(Value::Float(_)) if live().all(|v| matches!(v, Value::Float(_))) => FLOATS,
            Some(Value::Bool(_)) if live().all(|v| matches!(v, Value::Bool(_))) => BOOLS,
            Some(Value::Text(_)) if live().all(|v| matches!(v, Value::Text(_))) => TEXT,
            Some(Value::Char(_)) if live().all(|v| matches!(v, Value::Char(_))) => CHAR_TEXT,
            Some(f @ Value::Numeric(_))
                if dec_of(f).is_some_and(|(_, s)| {
                    live().all(|v| dec_of(v).is_some_and(|(_, vs)| vs == s))
                }) =>
            {
                DECIMALS
            }
            _ => VALUES,
        };
        let nulls: Vec<bool> = values.iter().map(Value::is_null).collect();
        let header = |out: &mut Vec<u8>, kind: u8| {
            out.push(kind);
            out.extend_from_slice(&(n as u32).to_le_bytes());
            if nulls.contains(&true) {
                out.push(1);
                bitmap(nulls.iter().copied(), out);
            } else {
                out.push(0);
            }
        };
        match kind {
            INTS => {
                let k = int_of(first.expect("a value")).expect("an integer").0;
                let ints: Vec<i64> = values
                    .iter()
                    .map(|v| int_of(v).map_or(0, |(_, x)| x))
                    .collect();
                let runs = 1 + ints.windows(2).filter(|w| w[0] != w[1]).count();
                let lo = ints.iter().copied().min().unwrap_or(0);
                let hi = ints.iter().copied().max().unwrap_or(0);
                let bits = bits_for((hi as i128 - lo as i128) as u64);
                if runs * 12 < n * bits as usize / 8 {
                    header(&mut out, RUNS);
                    out.push(k);
                    out.extend_from_slice(&(runs as u32).to_le_bytes());
                    let mut i = 0;
                    while i < n {
                        let mut j = i;
                        while j < n && ints[j] == ints[i] {
                            j += 1;
                        }
                        out.extend_from_slice(&ints[i].to_le_bytes());
                        out.extend_from_slice(&((j - i) as u32).to_le_bytes());
                        i = j;
                    }
                } else {
                    header(&mut out, INTS);
                    out.push(k);
                    out.extend_from_slice(&lo.to_le_bytes());
                    out.push(bits);
                    pack(
                        ints.iter().map(|&x| x.wrapping_sub(lo) as u64),
                        bits,
                        &mut out,
                    );
                }
            }
            FLOATS => {
                header(&mut out, FLOATS);
                for v in values {
                    let f = if let Value::Float(f) = v { *f } else { 0.0 };
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
            BOOLS => {
                header(&mut out, BOOLS);
                bitmap(
                    values.iter().map(|v| matches!(v, Value::Bool(true))),
                    &mut out,
                );
            }
            TEXT | CHAR_TEXT => {
                fn text(v: &Value) -> &[u8] {
                    match v {
                        Value::Text(s) | Value::Char(s) => s.as_bytes(),
                        _ => &[],
                    }
                }
                let (dict_kind, text_kind) = if kind == CHAR_TEXT {
                    (CHAR_DICT, CHAR_TEXT)
                } else {
                    (DICT, TEXT)
                };
                let mut dict: Vec<&[u8]> =
                    values.iter().filter(|v| !v.is_null()).map(text).collect();
                dict.sort_unstable();
                dict.dedup();
                if dict.len() <= n / 4 && dict.len() < 1 << 20 {
                    header(&mut out, dict_kind);
                    out.extend_from_slice(&(dict.len() as u32).to_le_bytes());
                    for d in &dict {
                        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
                        out.extend_from_slice(d);
                    }
                    let bits = bits_for(dict.len().saturating_sub(1) as u64);
                    out.push(bits);
                    let code = |v: &Value| {
                        if v.is_null() {
                            0
                        } else {
                            dict.binary_search(&text(v)).expect("in the dictionary") as u64
                        }
                    };
                    pack(values.iter().map(code), bits, &mut out);
                } else {
                    header(&mut out, text_kind);
                    let total: usize = values.iter().map(|v| text(v).len()).sum();
                    out.extend_from_slice(&(total as u32).to_le_bytes());
                    let mut at = 0u32;
                    out.extend_from_slice(&at.to_le_bytes());
                    for v in values {
                        at += text(v).len() as u32;
                        out.extend_from_slice(&at.to_le_bytes());
                    }
                    for v in values {
                        out.extend_from_slice(text(v));
                    }
                }
            }
            DECIMALS => {
                let scale = dec_of(first.expect("a value")).expect("a decimal").1;
                let ms: Vec<i64> = values
                    .iter()
                    .map(|v| dec_of(v).map_or(0, |(m, _)| m))
                    .collect();
                let lo = ms.iter().copied().min().unwrap_or(0);
                let hi = ms.iter().copied().max().unwrap_or(0);
                let bits = bits_for((hi as i128 - lo as i128) as u64);
                header(&mut out, DECIMALS);
                out.push(scale as u8);
                out.extend_from_slice(&lo.to_le_bytes());
                out.push(bits);
                pack(
                    ms.iter().map(|&x| x.wrapping_sub(lo) as u64),
                    bits,
                    &mut out,
                );
            }
            _ => {
                header(&mut out, VALUES);
                for v in values {
                    encode_value(&mut out, v);
                }
            }
        }
        out
    }

    /// [`encode`] of `values` against the reference encoder and [`stats`].
    fn same_as_reference(values: &[Value]) -> Vec<u8> {
        let (b, st) = encode(values.to_vec());
        assert_eq!(b, reference(values), "the bytes of {values:?}");
        assert_eq!(
            format!("{st:?}"),
            format!("{:?}", stats(values)),
            "the statistics of {values:?}"
        );
        b
    }

    fn roundtrip(values: Vec<Value>) -> usize {
        let b = same_as_reference(&values);
        let v = decode(&b).unwrap();
        let back: Vec<Value> = (0..values.len()).map(|i| v.value(i)).collect();
        assert_eq!(back, values);
        // A point read of each row: what decoding the column gives.
        for (i, want) in values.iter().enumerate() {
            if let Some(got) = value_at(&b, i).unwrap() {
                assert_eq!(&got, want, "row {i}");
            }
        }
        assert!(value_at(&b, values.len()).is_err());
        b.len()
    }

    #[test]
    fn repeated_text_is_a_dictionary_read_a_row_at_a_time() {
        let modes = ["AIR", "MAIL", "RAIL", "SHIP", "TRUCK"];
        let values: Vec<Value> = (0..1000)
            .map(|i| {
                if i % 11 == 0 {
                    Value::Null
                } else {
                    Value::Text(modes[i % 5].into())
                }
            })
            .collect();
        roundtrip(values.clone());
        let b = encode(values.clone()).0;
        assert_eq!(b[0], DICT);
        assert_eq!(value_at(&b, 3).unwrap(), Some(Value::Text("SHIP".into())));
        assert_eq!(value_at(&b, 0).unwrap(), Some(Value::Null));
    }

    #[test]
    fn char_values_are_text_kinds_of_their_own() {
        let pad = |s: &str| Value::Char(format!("{s:<10}"));
        let modes: Vec<Value> = (0..1000)
            .map(|i| {
                if i % 13 == 0 {
                    Value::Null
                } else {
                    pad(["AIR", "MAIL", "SHIP"][i % 3])
                }
            })
            .collect();
        roundtrip(modes.clone());
        assert_eq!(encode(modes.clone()).0[0], CHAR_DICT);
        assert_eq!(
            value_at(&encode(modes.clone()).0, 2).unwrap(),
            Some(pad("SHIP"))
        );
        let unique: Vec<Value> = (0..100).map(|i| pad(&format!("c{i}"))).collect();
        roundtrip(unique.clone());
        assert_eq!(encode(unique.clone()).0[0], CHAR_TEXT);
        assert_eq!(
            value_at(&encode(unique.clone()).0, 7).unwrap(),
            Some(pad("c7"))
        );
    }

    #[test]
    fn decimals_of_one_scale_pack_their_unscaled_values() {
        let d = |m: i128, s: u32| Value::Numeric(crate::decimal::Decimal::new(m, s).unwrap());
        let prices: Vec<Value> = (0..1000)
            .map(|i| {
                if i % 9 == 0 {
                    Value::Null
                } else {
                    d(90_000 + i * 37, 2)
                }
            })
            .collect();
        assert!(roundtrip(prices.clone()) < 1000 * 4, "packed");
        assert_eq!(encode(prices.clone()).0[0], DECIMALS);
        assert!(value_at(&encode(prices.clone()).0, 5).unwrap().is_some());
        // Of two scales, or past 64 bits: each value as it is.
        let mixed = vec![d(15, 1), d(150, 2), Value::Null];
        roundtrip(mixed.clone());
        assert_eq!(encode(mixed.clone()).0[0], VALUES);
        let wide = vec![d(i128::from(i64::MAX) * 10, 0), d(1, 0)];
        roundtrip(wide.clone());
        assert_eq!(encode(wide.clone()).0[0], VALUES);
        roundtrip(vec![
            d(-5, 3),
            d(i128::from(i64::MIN), 3),
            d(i128::from(i64::MAX), 3),
        ]);
    }

    #[test]
    fn columns_round_trip_in_their_encodings() {
        roundtrip(vec![]);
        roundtrip(vec![Value::Null; 5]);
        let ints: Vec<Value> = (0..1000).map(|i| Value::Int(1_000_000 + i * 3)).collect();
        assert!(roundtrip(ints) < 1000 * 2);
        let runs: Vec<Value> = (0..1000).map(|i| Value::Int(i / 100)).collect();
        assert!(roundtrip(runs) < 200);
        roundtrip(vec![
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Null,
            Value::Int(0),
        ]);
        roundtrip((0..50).map(|i| Value::Date(i * 7 - 100)).collect());
        roundtrip((0..50).map(|i| Value::TimestampTz(i * 1_000_000)).collect());
        roundtrip(vec![Value::Int(1), Value::Date(2)]); // mixed: values
        roundtrip(vec![Value::Float(1.5), Value::Null, Value::Float(-0.25)]);
        roundtrip((0..100).map(|i| Value::Bool(i % 3 == 0)).collect());
        let names: Vec<Value> = (0..1000)
            .map(|i| Value::Text(format!("city {}", i % 7)))
            .collect();
        assert!(roundtrip(names) < 1000);
        roundtrip(
            (0..100)
                .map(|i| Value::Text(format!("unique é {i}")))
                .collect(),
        );
        roundtrip(vec![
            Value::Text(String::new()),
            Value::Null,
            Value::Text("x".into()),
        ]);
        roundtrip(vec![
            Value::Numeric(crate::decimal::Decimal::from_i64(5)),
            Value::Array(vec![Value::Int(1)]),
            Value::Null,
        ]);
        for bits in [1u8, 7, 13, 33, 64] {
            let xs: Vec<u64> = (0..77)
                .map(|i| (i * 2654435761u64) & ((1u128 << bits) - 1) as u64)
                .collect();
            let mut b = Vec::new();
            pack(xs.iter().copied(), bits, &mut b);
            assert_eq!(unpack(&b, xs.len(), bits).unwrap(), xs);
        }
    }

    /// Typed vectors encode as the values did, byte for byte, with the same
    /// statistics: random columns of every kind, `NULL`s, ties, `NaN` and
    /// `-0.0`, padded and unpadded `char(n)`, decimals past 64 bits.
    #[test]
    fn typed_encoding_is_the_value_encoding() {
        let mut state = 0x5eed_u64;
        let mut next = move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            crate::kernels::mix(state)
        };
        let d = |m: i128, s: u32| Value::Numeric(crate::decimal::Decimal::new(m, s).unwrap());
        for round in 0..400 {
            let n = (next() % 300) as usize;
            let kind = round % 11;
            let nulls = next() % 3;
            let spread = 1 + next() % 1000;
            let col: Vec<Value> = (0..n)
                .map(|_| {
                    let r = next();
                    if nulls > 0 && r % (2 + nulls * 3) == 0 {
                        return Value::Null;
                    }
                    let x = (r >> 8) % spread;
                    match kind {
                        0 => Value::Int(x as i64 - 500),
                        1 => Value::Int(if r % 2 == 0 {
                            i64::MIN
                        } else {
                            i64::MAX - x as i64
                        }),
                        2 => Value::Date(x as i32 - 300),
                        3 => Value::Timestamp((x as i64) * 1_000_000),
                        4 => Value::Float(match r % 7 {
                            0 => f64::NAN,
                            1 => -0.0,
                            2 => 0.0,
                            _ => x as f64 / 8.0 - 3.0,
                        }),
                        5 => Value::Bool(x % 2 == 0),
                        6 => Value::Text(format!("t{}", x % (1 + spread / 50))),
                        7 => Value::Char(format!(
                            "{:<w$}",
                            format!("c{x}"),
                            w = 4 + (r % 3) as usize
                        )),
                        8 => d(x as i128 * 37 - 900, 2),
                        9 => d(
                            if r % 5 == 0 {
                                i128::from(i64::MAX) * 3
                            } else {
                                x as i128
                            },
                            1,
                        ),
                        _ => match r % 3 {
                            0 => Value::Int(x as i64),
                            1 => Value::Text(x.to_string()),
                            _ => Value::Time(x as i64),
                        },
                    }
                })
                .collect();
            same_as_reference(&col);
        }
        // A dictionary at a quarter distinct, not past it.
        for distinct in [25, 26] {
            let col: Vec<Value> = (0..100)
                .map(|i| Value::Text(format!("v{}", i % distinct)))
                .collect();
            let b = same_as_reference(&col);
            assert_eq!(b[0], if distinct == 25 { DICT } else { TEXT });
        }
    }

    #[test]
    fn damaged_segments_are_errors() {
        let b = encode(
            (0..100)
                .map(|i| Value::Text(format!("t{}", i % 3)))
                .collect::<Vec<_>>(),
        )
        .0;
        for cut in 0..b.len() {
            let _ = decode(&b[..cut]); // no panic
        }
        let mut bad = b.clone();
        bad[0] = 99;
        assert!(decode(&bad).is_err());
    }
}
