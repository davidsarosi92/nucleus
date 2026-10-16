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
//! | double | `03`, then the IEEE bits, sign-flipped (positive) or inverted (negative), big-endian; `-0` as `0`, one `NaN` above all numbers |
//! | text | `04`, the bytes with `00` escaped as `00 FF`, then `00 01` |
//!
//! Every encoding is self-delimiting, so a key can be split back into its
//! values (`decode_key_value`).
//!
//! **Rows** — `01` (version), column count `u16`, then per column a tag
//! (`0` null, `1` bool, `2` bigint, `3` double, `4` text) and its bytes
//! (little-endian; text with a `u32` length).  A row shorter than its table
//! (a column added later) reads the missing columns as `NULL`.

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
            for &b in s.as_bytes() {
                out.push(b);
                if b == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x01]);
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
            let mut s = Vec::new();
            let mut i = 1;
            loop {
                match (b.get(i), b.get(i + 1)) {
                    (Some(0), Some(0xFF)) => {
                        s.push(0);
                        i += 2;
                    }
                    (Some(0), Some(1)) => break,
                    (Some(c), _) if *c != 0 => {
                        s.push(*c);
                        i += 1;
                    }
                    _ => return Err(bad_key()),
                }
            }
            (
                Value::Text(String::from_utf8(s).map_err(|_| bad_key())?),
                i + 2,
            )
        }
        _ => return Err(bad_key()),
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

const ROW_VERSION: u8 = 1;

/// Encodes a row.
pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + row.len() * 9);
    out.push(ROW_VERSION);
    out.extend_from_slice(&(row.len() as u16).to_le_bytes());
    for v in row {
        match v {
            Value::Null => out.push(0),
            Value::Bool(b) => out.extend_from_slice(&[1, u8::from(*b)]),
            Value::Int(i) => {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
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
        }
    }
    out
}

/// Decodes a row of a table with `columns` columns.
pub fn decode_row(b: &[u8], columns: usize) -> Result<Vec<Value>> {
    let bad = || Error::internal("malformed row encoding");
    let mut r = Reader::new(b);
    if r.u8().map_err(|_| bad())? != ROW_VERSION {
        return Err(bad());
    }
    let n = r.u16().map_err(|_| bad())? as usize;
    let mut row = Vec::with_capacity(columns.max(n));
    for _ in 0..n {
        row.push(match r.u8().map_err(|_| bad())? {
            0 => Value::Null,
            1 => Value::Bool(r.u8().map_err(|_| bad())? == 1),
            2 => Value::Int(r.u64().map_err(|_| bad())? as i64),
            3 => Value::Float(f64::from_bits(r.u64().map_err(|_| bad())?)),
            4 => Value::Text(r.str().map_err(|_| bad())?),
            _ => return Err(bad()),
        });
    }
    row.resize(columns.max(n), Value::Null);
    Ok(row)
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
        let row = encode_row(std::slice::from_ref(v));
        self.0.extend_from_slice(&row[3..]);
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

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
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
        let mut row = vec![ROW_VERSION, 1, 0];
        // Re-frame as a one-column row and decode it.
        let start = self.b;
        let tag = self.u8()?;
        let len = match tag {
            0 => 0,
            1 => 1,
            2 | 3 => 8,
            4 => {
                4 + u32::from_le_bytes(
                    self.b
                        .get(..4)
                        .ok_or_else(|| Error::internal("truncated value"))?
                        .try_into()
                        .unwrap(),
                ) as usize
            }
            _ => return Err(Error::internal("bad value tag")),
        };
        self.take(len)?;
        row.extend_from_slice(&start[..1 + len]);
        Ok(decode_row(&row, 1)?.remove(0))
    }
    /// Whether everything was read.
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
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
        ]
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
