//! Byte encodings: order-preserving keys, rows, and a small writer/reader
//! for catalog records.
//!
//! **Keys** (PLAN.md principle 2) — bytewise order of encoded values is SQL
//! order, `NULL` first; a composite key is the concatenation:
//!
//! | Value | Encoding |
//! |-------|----------|
//! | `NULL` | `00` |
//! | bool | `01`, then `00`/`01` |
//! | bigint | `02`, then `i ^ i64::MIN` as 8 bytes big-endian |
//! | double, real | `03`, then the IEEE bits (a real's widened to 64), sign-flipped (positive) or inverted (negative), big-endian; `-0` as `0`, one `NaN` above all numbers |
//! | text | `04`, the bytes with `00` escaped as `00 FF`, then `00 01` |
//! | bytea | `0C`, the bytes escaped as text's |
//! | time | `0D`, then µs since midnight as 8 bytes big-endian |
//! | char(n) | `0E`, without its trailing blanks, escaped as text's |
//! | enum | `10`, its sort key escaped as text's ([`crate::enums`]) |
//! | inet, cidr | `0F`, then the family (`04` / `06`), the address masked to its mask length, the mask length, the address ([`crate::inet`]) |
//! | numeric | `05`, then `02` for zero; else `03` (positive) and the leading digit's power of ten as `i16 ^ 0x8000` big-endian, the significant digits as bytes `d + 1`, then `00`; or `01` (negative) and the positive form's bytes inverted |
//! | date | `06`, then `days ^ i32::MIN` as 4 bytes big-endian |
//! | timestamp, timestamptz | `07` / `08`, then `µs ^ i64::MIN` as 8 bytes big-endian |
//! | interval | `09`, then its span (months as 30 days) in µs as `i128 ^ i128::MIN`, 16 bytes big-endian |
//! | array | `0A`, then per element `01` and its encoding — or `02` for a `NULL` element, which sorts after the others as in comparisons — then `00` (key encoding version 2; version 1 wrote a `NULL` element as `01 00`, which still decodes) |
//! | registered type | `0B`, its stable id (`u32` big-endian, [`crate::ext::type_id_of`]), then its own key encoding — or, for a type without one, its canonical bytes escaped as text's |
//!
//! Every encoding is self-delimiting, so a key can be split back into its
//! values (`decode_key_value`).
//!
//! **Rows** — `02` (version), column count `u16`, then per column a tag
//! (`0` null, `1` bool, `2` an integer in 8 bytes, `12` in 2, `13` in 4 —
//! the fewest that hold the value, whatever the column's width — `3` double,
//! `14` real (4 bytes), `15` bytea (a `u32` length and the bytes), `16` a
//! value stored out of line (a `u32` length: its encoding is in chunks,
//! [`crate::large`]), `17` time (`i64` µs), `18` char(n) (as text, padded), `19` inet and `20` cidr (the family, mask length and address), `21` an
//! enum (its sort key), `4` text, `5` numeric:
//! `i128` mantissa and `u8` scale, `6` date `i32`, `7` timestamp and `8`
//! timestamptz `i64`, `9` interval: months `i32`, days `i32`, µs `i64`,
//! `10` array: a `u32` count, then each element tagged the same way; `11` a
//! registered type: its stable id `u32`, its encoding version `u16`, a
//! `u32` length and its canonical bytes) and its bytes (little-endian; text
//! with a `u32` length).  A row shorter than its table
//! (a column added later) reads the missing columns as `NULL`.  Version 1
//! (before tags 12 and 13) still reads.
//!
//! Keys hold every integer in 8 bytes, whatever its type: a `smallint`
//! and a `bigint` that are equal have equal keys, so an index serves a
//! comparison across widths and widening a column keeps its keys.

use crate::datetime::{Interval, USECS_PER_DAY};
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::types::Value;

/// Appends the order-preserving encoding of `v` to `out`.
pub fn encode_key_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0x00),
        Value::Bool(b) => out.extend_from_slice(&[0x01, u8::from(*b)]),
        Value::Int(i) => {
            out.push(0x02);
            out.extend_from_slice(&((*i ^ i64::MIN) as u64).to_be_bytes());
        }
        Value::Real(f) => encode_key_value(out, &Value::Float(f64::from(*f))),
        Value::Float(f) => {
            out.push(0x03);
            let f = if *f == 0.0 {
                0.0
            } else if f.is_nan() {
                f64::NAN
            } else {
                *f
            };
            let bits = f.to_bits();
            let ordered = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Text(s) => {
            out.push(0x04);
            escape(out, s.as_bytes());
        }
        Value::Bytea(b) => {
            out.push(0x0C);
            escape(out, b);
        }
        Value::Inet(n) | Value::Cidr(n) => {
            out.push(0x0F);
            n.key(out);
        }
        Value::Enum(e) => {
            out.push(0x10);
            escape(out, &e.key);
        }
        Value::Char(s) => {
            out.push(0x0E);
            escape(out, crate::types::unpadded(s).as_bytes());
        }
        Value::Time(t) => {
            out.push(0x0D);
            out.extend_from_slice(&((*t ^ i64::MIN) as u64).to_be_bytes());
        }
        Value::Ext(t, bytes) => {
            out.push(0x0B);
            out.extend_from_slice(&t.id().to_be_bytes());
            match t.imp().key_encoding() {
                Some(k) => k.encode(v, out),
                None => escape(out, bytes),
            }
        }
        Value::Numeric(d) => {
            out.push(0x05);
            match d.digits() {
                None => out.push(0x02),
                Some((digits, exp)) => {
                    let mut body = Vec::with_capacity(digits.len() + 3);
                    body.extend_from_slice(&((exp as i16 as u16) ^ 0x8000).to_be_bytes());
                    body.extend(digits.iter().map(|d| d + 1));
                    body.push(0x00);
                    if d.signum() > 0 {
                        out.push(0x03);
                        out.extend_from_slice(&body);
                    } else {
                        out.push(0x01);
                        out.extend(body.iter().map(|b| !b));
                    }
                }
            }
        }
        Value::Date(d) => {
            out.push(0x06);
            out.extend_from_slice(&((*d ^ i32::MIN) as u32).to_be_bytes());
        }
        Value::Timestamp(t) | Value::TimestampTz(t) => {
            out.push(if matches!(v, Value::Timestamp(_)) {
                0x07
            } else {
                0x08
            });
            out.extend_from_slice(&((*t ^ i64::MIN) as u64).to_be_bytes());
        }
        Value::Interval(i) => {
            out.push(0x09);
            out.extend_from_slice(&((i.span() ^ i128::MIN) as u128).to_be_bytes());
        }
        Value::Array(items) => {
            // Each element after a 01, a NULL element as 02, then 00:
            // element order (a NULL element after the others, as in
            // comparisons), then the shorter first.
            out.push(0x0A);
            for item in items {
                if item.is_null() {
                    out.push(0x02);
                } else {
                    out.push(0x01);
                    encode_key_value(out, item);
                }
            }
            out.push(0x00);
        }
    }
}

/// `b` with `00` escaped as `00 FF`, then `00 01`: self-delimiting, and in
/// `b`'s bytewise order.
fn escape(out: &mut Vec<u8>, b: &[u8]) {
    for &x in b {
        out.push(x);
        if x == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}

/// The bytes [`escape`] wrote at the front of `b`, and how many it used.
fn unescape(b: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut s = Vec::new();
    let mut i = 0;
    loop {
        match (b.get(i), b.get(i + 1)) {
            (Some(0), Some(0xFF)) => {
                s.push(0);
                i += 2;
            }
            (Some(0), Some(1)) => return Ok((s, i + 2)),
            (Some(c), _) if *c != 0 => {
                s.push(*c);
                i += 1;
            }
            _ => return Err(bad_key()),
        }
    }
}

/// The encoding of several values, concatenated.
pub fn encode_key(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        encode_key_value(&mut out, v);
    }
    out
}

fn bad_key() -> Error {
    Error::internal("malformed key encoding")
}

/// Decodes one value from the front of `b`; returns it and the bytes used.
pub fn decode_key_value(b: &[u8]) -> Result<(Value, usize)> {
    let eight = |b: &[u8]| -> Result<u64> {
        Ok(u64::from_be_bytes(
            b.get(1..9).ok_or_else(bad_key)?.try_into().unwrap(),
        ))
    };
    Ok(match b.first().ok_or_else(bad_key)? {
        0x00 => (Value::Null, 1),
        0x01 => (Value::Bool(*b.get(1).ok_or_else(bad_key)? == 1), 2),
        0x02 => (Value::Int((eight(b)? as i64) ^ i64::MIN), 9),
        0x03 => {
            let o = eight(b)?;
            let bits = if o >> 63 == 1 { o ^ (1 << 63) } else { !o };
            (Value::Float(f64::from_bits(bits)), 9)
        }
        0x04 => {
            let (s, used) = unescape(&b[1..])?;
            (
                Value::Text(String::from_utf8(s).map_err(|_| bad_key())?),
                1 + used,
            )
        }
        0x10 => {
            let (k, used) = unescape(&b[1..])?;
            (unresolved_enum(&k), 1 + used)
        }
        0x0F => {
            let (n, used) = crate::inet::Net::from_key(&b[1..]).ok_or_else(bad_key)?;
            (Value::Inet(n), 1 + used)
        }
        0x0E => {
            let (s, used) = unescape(&b[1..])?;
            (
                Value::Char(String::from_utf8(s).map_err(|_| bad_key())?),
                1 + used,
            )
        }
        0x0D => (Value::Time((eight(b)? as i64) ^ i64::MIN), 9),
        0x0C => {
            let (b, used) = unescape(&b[1..])?;
            (Value::Bytea(b.into()), 1 + used)
        }
        0x0B => {
            let id = u32::from_be_bytes(b.get(1..5).ok_or_else(bad_key)?.try_into().unwrap());
            let t = crate::ext::TypeRef::by_id(id);
            match t.imp().key_encoding() {
                Some(k) => {
                    let (v, used) = k.decode(&b[5..])?;
                    (v, 5 + used)
                }
                None => {
                    let (bytes, used) = unescape(&b[5..])?;
                    (Value::ext(t, bytes), 5 + used)
                }
            }
        }
        0x05 => match b.get(1).ok_or_else(bad_key)? {
            0x02 => (Value::Numeric(Decimal::ZERO), 2),
            &class @ (0x01 | 0x03) => {
                let neg = class == 0x01;
                let byte = |i: usize| -> Result<u8> {
                    let x = *b.get(i).ok_or_else(bad_key)?;
                    Ok(if neg { !x } else { x })
                };
                let exp = i64::from((u16::from_be_bytes([byte(2)?, byte(3)?]) ^ 0x8000) as i16);
                let mut digits = Vec::new();
                let mut i = 4;
                loop {
                    match byte(i)? {
                        0 => break,
                        d @ 1..=10 => digits.push(d - 1),
                        _ => return Err(bad_key()),
                    }
                    i += 1;
                }
                (
                    Value::Numeric(Decimal::from_digits(neg, &digits, exp).map_err(|_| bad_key())?),
                    i + 1,
                )
            }
            _ => return Err(bad_key()),
        },
        0x06 => {
            let x = u32::from_be_bytes(b.get(1..5).ok_or_else(bad_key)?.try_into().unwrap());
            (Value::Date((x as i32) ^ i32::MIN), 5)
        }
        0x07 => (Value::Timestamp((eight(b)? as i64) ^ i64::MIN), 9),
        0x08 => (Value::TimestampTz((eight(b)? as i64) ^ i64::MIN), 9),
        0x0A => {
            let mut items = Vec::new();
            let mut at = 1;
            loop {
                match b.get(at).ok_or_else(bad_key)? {
                    0x00 => break,
                    0x01 => {
                        let (v, used) = decode_key_value(&b[at + 1..])?;
                        items.push(v);
                        at += 1 + used;
                    }
                    0x02 => {
                        items.push(Value::Null);
                        at += 1;
                    }
                    _ => return Err(bad_key()),
                }
            }
            (Value::Array(items), at + 1)
        }
        0x09 => {
            let x = u128::from_be_bytes(b.get(1..17).ok_or_else(bad_key)?.try_into().unwrap());
            let span = (x as i128) ^ i128::MIN;
            // The span is all a key keeps: days and time.
            let day = i128::from(USECS_PER_DAY);
            let days = i32::try_from(span.div_euclid(day)).map_err(|_| bad_key())?;
            (
                Value::Interval(Interval {
                    months: 0,
                    days,
                    micros: span.rem_euclid(day) as i64,
                }),
                17,
            )
        }
        _ => return Err(bad_key()),
    })
}

/// An enum value with its sort key alone: reading the row fills in its
/// type and label ([`crate::enums::Labels`]).
fn unresolved_enum(key: &[u8]) -> Value {
    Value::Enum(crate::enums::EnumValue {
        ty: None,
        key: key.into(),
        label: "".into(),
    })
}

/// Decodes `n` values from the front of `b`; returns them and the bytes used.
pub fn decode_key(b: &[u8], n: usize) -> Result<(Vec<Value>, usize)> {
    let mut values = Vec::with_capacity(n);
    let mut at = 0;
    for _ in 0..n {
        let (v, used) = decode_key_value(&b[at..])?;
        values.push(v);
        at += used;
    }
    Ok((values, at))
}

/// Version 2 added tags 12 to 21; version 1 rows still read.
pub const ROW_VERSION: u8 = 2;

/// Encodes a row.
pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + row.len() * 9);
    out.push(ROW_VERSION);
    out.extend_from_slice(&(row.len() as u16).to_le_bytes());
    for v in row {
        encode_value(&mut out, v);
    }
    out
}

/// Appends one value's row encoding: its tag, then its bytes.
pub fn encode_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => out.extend_from_slice(&[1, u8::from(*b)]),
        Value::Int(i) => {
            if let Ok(x) = i16::try_from(*i) {
                out.push(12);
                out.extend_from_slice(&x.to_le_bytes());
            } else if let Ok(x) = i32::try_from(*i) {
                out.push(13);
                out.extend_from_slice(&x.to_le_bytes());
            } else {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
        Value::Real(f) => {
            out.push(14);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Float(f) => {
            out.push(3);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Enum(e) => {
            out.push(21);
            out.extend_from_slice(&(e.key.len() as u32).to_le_bytes());
            out.extend_from_slice(&e.key);
        }
        Value::Inet(n) | Value::Cidr(n) => {
            out.push(if matches!(v, Value::Inet(_)) { 19 } else { 20 });
            out.extend(n.to_bytes());
        }
        Value::Char(s) => {
            out.push(18);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Time(t) => {
            out.push(17);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Bytea(b) => {
            out.push(15);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Numeric(d) => {
            out.push(5);
            out.extend_from_slice(&d.mantissa().to_le_bytes());
            out.push(d.scale() as u8);
        }
        Value::Date(d) => {
            out.push(6);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            out.push(7);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::TimestampTz(t) => {
            out.push(8);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Interval(i) => {
            out.push(9);
            out.extend_from_slice(&i.months.to_le_bytes());
            out.extend_from_slice(&i.days.to_le_bytes());
            out.extend_from_slice(&i.micros.to_le_bytes());
        }
        Value::Array(items) => {
            out.push(10);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                encode_value(out, item);
            }
        }
        Value::Ext(t, bytes) => {
            out.push(11);
            out.extend_from_slice(&t.id().to_le_bytes());
            out.extend_from_slice(&t.imp().encoding_version().to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

/// Reads one value's row encoding.
pub fn decode_value(r: &mut Reader) -> Result<Value> {
    let bad = |_| Error::internal("malformed row encoding");
    Ok(match r.u8().map_err(bad)? {
        0 => Value::Null,
        1 => Value::Bool(r.u8().map_err(bad)? == 1),
        2 => Value::Int(r.u64().map_err(bad)? as i64),
        18 => Value::Char(r.str().map_err(bad)?),
        21 => {
            let n = r.u32().map_err(bad)? as usize;
            unresolved_enum(r.take(n).map_err(bad)?)
        }
        tag @ (19 | 20) => {
            let family = r.u8().map_err(bad)?;
            let n = if family == 6 { 16 } else { 4 };
            let rest = r.take(1 + n).map_err(bad)?;
            let mut b = vec![family];
            b.extend_from_slice(rest);
            let net = crate::inet::Net::from_bytes(&b)
                .ok_or_else(|| Error::internal("malformed row encoding"))?;
            if tag == 19 {
                Value::Inet(net)
            } else {
                Value::Cidr(net)
            }
        }
        17 => Value::Time(r.u64().map_err(bad)? as i64),
        15 => {
            let n = r.u32().map_err(bad)? as usize;
            Value::Bytea(r.take(n).map_err(bad)?.into())
        }
        14 => Value::Real(f32::from_bits(r.u32().map_err(bad)?)),
        12 => Value::Int(i64::from(r.u16().map_err(bad)? as i16)),
        13 => Value::Int(i64::from(r.u32().map_err(bad)? as i32)),
        3 => Value::Float(f64::from_bits(r.u64().map_err(bad)?)),
        4 => Value::Text(r.str().map_err(bad)?),
        5 => {
            let lo = r.u64().map_err(bad)?;
            let hi = r.u64().map_err(bad)?;
            let m = (i128::from(hi as i64) << 64) | i128::from(lo);
            let scale = r.u8().map_err(bad)?;
            Value::Numeric(Decimal::new(m, u32::from(scale)).map_err(bad)?)
        }
        6 => Value::Date(r.u32().map_err(bad)? as i32),
        7 => Value::Timestamp(r.u64().map_err(bad)? as i64),
        8 => Value::TimestampTz(r.u64().map_err(bad)? as i64),
        9 => Value::Interval(Interval {
            months: r.u32().map_err(bad)? as i32,
            days: r.u32().map_err(bad)? as i32,
            micros: r.u64().map_err(bad)? as i64,
        }),
        10 => {
            let n = r.u32().map_err(bad)? as usize;
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                items.push(decode_value(r)?);
            }
            Value::Array(items)
        }
        11 => {
            let t = crate::ext::TypeRef::by_id(r.u32().map_err(bad)?);
            let version = r.u16().map_err(bad)?;
            let n = r.u32().map_err(bad)? as usize;
            let bytes = r.take(n).map_err(bad)?;
            let current = t.imp().encoding_version();
            if version == current || r#missing(t) {
                Value::ext(t, bytes)
            } else if version < current {
                Value::ext(t, t.imp().upgrade(version, bytes)?)
            } else {
                return Err(Error::feature(format!(
                    "a {} value is stored in encoding version {version}, newer than this \
                     build's ({current}); upgrade nucleus",
                    t.name()
                )));
            }
        }
        _ => return Err(Error::internal("malformed row encoding")),
    })
}

/// Whether `t` is the stand-in for a type this process lacks (whose bytes
/// are kept as they are).
fn r#missing(t: crate::ext::TypeRef) -> bool {
    crate::ext::is_missing(t)
}

/// Decodes a row of a table with `columns` columns.
pub fn decode_row(b: &[u8], columns: usize) -> Result<Vec<Value>> {
    decode_row_with(b, columns, &mut |_, _| {
        Err(Error::internal(
            "a value stored out of line, read without its table",
        ))
    })
}

/// The tag of a value stored out of line ([`crate::large`]).
pub const OUT_OF_LINE: u8 = 16;

/// Decodes a row; `resolve(column, length)` reads each value stored out of
/// line (tag 16).
pub fn decode_row_with(
    b: &[u8],
    columns: usize,
    resolve: &mut dyn FnMut(usize, u32) -> Result<Value>,
) -> Result<Vec<Value>> {
    decode_some(b, columns, None, resolve)
}

/// [`decode_row_with`], decoding only the columns `wanted` marks (`None`:
/// all): the others are skipped without being built (or fetched, if out of
/// line) and read as NULL.
pub fn decode_some(
    b: &[u8],
    columns: usize,
    wanted: Option<&[bool]>,
    resolve: &mut dyn FnMut(usize, u32) -> Result<Value>,
) -> Result<Vec<Value>> {
    let bad = |_| Error::internal("malformed row encoding");
    let mut r = Reader::new(b);
    if !(1..=ROW_VERSION).contains(&r.u8().map_err(bad)?) {
        return Err(Error::internal("malformed row encoding"));
    }
    let n = r.u16().map_err(bad)? as usize;
    let mut row = Vec::with_capacity(columns.max(n));
    for c in 0..n {
        let want = wanted.is_none_or(|w| w.get(c).copied().unwrap_or(false));
        if r.peek() == Some(OUT_OF_LINE) {
            r.u8().map_err(bad)?;
            let len = r.u32().map_err(bad)?;
            row.push(if want { resolve(c, len)? } else { Value::Null });
        } else if want {
            row.push(decode_value(&mut r)?);
        } else {
            skip_value(&mut r)?;
            row.push(Value::Null);
        }
    }
    row.resize(columns.max(n), Value::Null);
    Ok(row)
}

/// Moves past one value's row encoding without building it.
fn skip_value(r: &mut Reader) -> Result<()> {
    let bad = |_| Error::internal("malformed row encoding");
    let fixed = |r: &mut Reader, n: usize| r.take(n).map(|_| ()).map_err(bad);
    match r.peek() {
        Some(0) => fixed(r, 1),
        Some(1) => fixed(r, 2),
        Some(12) => fixed(r, 3),
        Some(13 | 14 | 6) => fixed(r, 5),
        Some(2 | 3 | 7 | 8 | 17) => fixed(r, 9),
        Some(9) => fixed(r, 17),
        Some(5) => fixed(r, 18),
        Some(4 | 18 | 15 | 21) => {
            r.u8().map_err(bad)?;
            let n = r.u32().map_err(bad)? as usize;
            fixed(r, n)
        }
        // Arrays, registered types, addresses: decoded and dropped.
        _ => decode_value(r).map(|_| ()),
    }
}

/// The columns of an encoded row stored out of line, and their lengths.
pub fn out_of_line(b: &[u8]) -> Result<Vec<(usize, u32)>> {
    let mut out = Vec::new();
    decode_row_with(b, 0, &mut |c, len| {
        out.push((c, len));
        Ok(Value::Null)
    })?;
    Ok(out)
}

/// A little-endian record writer (catalog entries, statistics).
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    /// A `u8`.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    /// A `u16`.
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// A `u32`.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// A `u64`.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// A length-prefixed string.
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    /// An optional string.
    pub fn opt_str(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            None => self.u8(0),
            Some(s) => self.u8(1).str(s),
        }
    }
    /// A value, row-encoded.
    pub fn value(&mut self, v: &Value) -> &mut Self {
        encode_value(&mut self.0, v);
        self
    }
}

/// Reads what [`Writer`] wrote.
pub struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over `b`.
    pub fn new(b: &'a [u8]) -> Self {
        Self { b }
    }

    /// The next byte, not consumed.
    pub fn peek(&self) -> Option<u8> {
        self.b.first().copied()
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(Error::internal("truncated record"));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    /// A `u8`.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    /// A `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    /// A `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    /// A `u64`.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// A length-prefixed string.
    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| Error::internal("record string not UTF-8"))
    }
    /// An optional string.
    pub fn opt_str(&mut self) -> Result<Option<String>> {
        Ok(if self.u8()? == 1 {
            Some(self.str()?)
        } else {
            None
        })
    }
    /// A value.
    pub fn value(&mut self) -> Result<Value> {
        decode_value(self)
    }
    /// Whether everything was read.
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }
    /// How many bytes are left.
    pub fn remaining(&self) -> usize {
        self.b.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(7),
            Value::Int(i64::MAX),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-2.5),
            Value::Float(-0.0),
            Value::Float(1e-300),
            Value::Float(3.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
            Value::Text(String::new()),
            Value::Text("\0".into()),
            Value::Text("\0\0".into()),
            Value::Text("\0a".into()),
            Value::Text("a".into()),
            Value::Text("a\0".into()),
            Value::Text("ab".into()),
            Value::Text("é".into()),
            num("-1000"),
            num("-12.5"),
            num("-12.25"),
            num("-12.2"),
            num("-0.001"),
            num("0"),
            num("0.001"),
            num("1"),
            num("1.5"),
            num("1.55"),
            num("9.99"),
            num("10"),
            num("123456789012345678901234567890.12345678"),
            Value::Date(-2_400_000),
            Value::Date(-1),
            Value::Date(0),
            Value::Date(9000),
            Value::Timestamp(-5),
            Value::Timestamp(0),
            Value::Timestamp(1 << 40),
            Value::TimestampTz(-1),
            Value::TimestampTz(7),
            Value::Interval(Interval {
                months: -1,
                days: 0,
                micros: 0,
            }),
            Value::Interval(Interval {
                months: 0,
                days: 1,
                micros: -1,
            }),
            Value::Interval(Interval {
                months: 0,
                days: 1,
                micros: 0,
            }),
            Value::Interval(Interval {
                months: 1,
                days: 0,
                micros: 5,
            }),
        ]
    }

    fn num(s: &str) -> Value {
        Value::Numeric(Decimal::parse(s).unwrap())
    }

    #[test]
    fn key_order_matches_value_order_within_a_type() {
        let vals = sample();
        for a in &vals {
            for b in &vals {
                if a.data_type() != b.data_type() || a.is_null() || b.is_null() {
                    continue;
                }
                let (ka, kb) = (
                    encode_key(std::slice::from_ref(a)),
                    encode_key(std::slice::from_ref(b)),
                );
                assert_eq!(ka.cmp(&kb), a.sort_cmp(b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn composite_keys_order_and_split() {
        let pairs = [
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("a".into()), Value::Int(10)],
            vec![Value::Text("a\0".into()), Value::Int(1)],
            vec![Value::Text("ab".into()), Value::Int(0)],
        ];
        for w in pairs.windows(2) {
            assert!(encode_key(&w[0]) < encode_key(&w[1]));
        }
        for p in &pairs {
            let k = encode_key(p);
            let (vals, used) = decode_key(&k, 2).unwrap();
            assert_eq!((&vals, used), (p, k.len()));
        }
        for v in sample() {
            let k = encode_key(std::slice::from_ref(&v));
            let (d, _) = decode_key_value(&k).unwrap();
            assert!(d == v || (d.is_null() && v.is_null()), "{v:?}");
        }
        // -0 and 0 are the same key.
        assert_eq!(
            encode_key(&[Value::Float(-0.0)]),
            encode_key(&[Value::Float(0.0)])
        );
    }

    #[test]
    fn rows_roundtrip_and_grow() {
        let row = sample();
        let decoded = decode_row(&encode_row(&row), row.len()).unwrap();
        assert_eq!(decoded.len(), row.len());
        for (a, b) in decoded.iter().zip(&row) {
            assert!(a == b || (a.is_null() && b.is_null()));
        }
        let wider = decode_row(&encode_row(&[Value::Int(1)]), 3).unwrap();
        assert_eq!(wider, vec![Value::Int(1), Value::Null, Value::Null]);
        assert!(decode_row(b"\x02", 1).is_err());
    }

    #[test]
    fn records() {
        let mut w = Writer::default();
        w.u8(7)
            .u32(9)
            .str("héllo")
            .opt_str(None)
            .opt_str(Some("x"))
            .value(&Value::Text("v".into()))
            .value(&Value::Int(-3));
        let mut r = Reader::new(&w.0);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 9);
        assert_eq!(r.str().unwrap(), "héllo");
        assert_eq!(r.opt_str().unwrap(), None);
        assert_eq!(r.opt_str().unwrap().as_deref(), Some("x"));
        assert_eq!(r.value().unwrap(), Value::Text("v".into()));
        assert_eq!(r.value().unwrap(), Value::Int(-3));
        assert!(r.is_empty());
    }
}
