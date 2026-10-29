//! Column segments' encodings (PLAN.md §M14.20 V7): one column of a
//! segment's rows as one value, encoded by what its values are —
//!
//! - integers, dates, times and timestamps: frame of reference and bit
//!   packing (each value less the least, in as many bits as the largest
//!   difference needs), or runs of equal values when there are few;
//! - doubles as they are, booleans a bit each;
//! - text: a dictionary and bit-packed codes when values repeat, else
//!   offsets and bytes (Arrow's layout);
//! - anything else: each value's row encoding, back to back.
//!
//! `NULL`s are a bitmap beside the values.  Decoding gives the vectorized
//! executor's vectors directly ([`Vector`]), typed as a scan of the row
//! storage gives them.

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

fn bad() -> Error {
    Error::internal("a malformed column segment")
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

/// One column of a segment, encoded.
pub fn encode(values: &[Value]) -> Vec<u8> {
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
        TEXT => {
            fn text(v: &Value) -> &[u8] {
                match v {
                    Value::Text(s) => s.as_bytes(),
                    _ => &[],
                }
            }
            let mut dict: Vec<&[u8]> = values.iter().filter(|v| !v.is_null()).map(text).collect();
            dict.sort_unstable();
            dict.dedup();
            if dict.len() <= n / 4 && dict.len() < 1 << 20 {
                header(&mut out, DICT);
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
                header(&mut out, TEXT);
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
        _ => {
            header(&mut out, VALUES);
            for v in values {
                encode_value(&mut out, v);
            }
        }
    }
    out
}

/// A column [`encode`]d, as a vector.
pub fn decode(b: &[u8]) -> Result<Vector> {
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
        DICT => {
            let m = r.u32().map_err(|_| bad())? as usize;
            let mut dict = Vec::with_capacity(m.min(n));
            for _ in 0..m {
                let len = r.u32().map_err(|_| bad())? as usize;
                dict.push(r.take(len).map_err(|_| bad())?);
            }
            let bits = r.u8().map_err(|_| bad())?;
            let rest = r.take(r.remaining()).map_err(|_| bad())?;
            let codes = unpack(rest, n, bits)?;
            let mut offsets = Vec::with_capacity(n + 1);
            let mut bytes = Vec::new();
            offsets.push(0u32);
            for (i, &c) in codes.iter().enumerate() {
                if !nulls.as_ref().is_some_and(|z| z[i]) {
                    bytes.extend_from_slice(dict.get(c as usize).ok_or_else(bad)?);
                }
                offsets.push(bytes.len() as u32);
            }
            check_utf8(&bytes)?;
            Ok(Vector::Text(offsets, bytes, nulls))
        }
        TEXT => {
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

    fn roundtrip(values: Vec<Value>) -> usize {
        let b = encode(&values);
        let v = decode(&b).unwrap();
        let back: Vec<Value> = (0..values.len()).map(|i| v.value(i)).collect();
        assert_eq!(back, values);
        b.len()
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

    #[test]
    fn damaged_segments_are_errors() {
        let b = encode(
            &(0..100)
                .map(|i| Value::Text(format!("t{}", i % 3)))
                .collect::<Vec<_>>(),
        );
        for cut in 0..b.len() {
            let _ = decode(&b[..cut]); // no panic
        }
        let mut bad = b.clone();
        bad[0] = 99;
        assert!(decode(&bad).is_err());
    }
}
