//! The columnar result format (`ncp` version 7, DOCS §10): a batch of
//! rows sent as one buffer per column, laid out so a driver can hand the
//! buffers over without building a value per cell — numpy arrays, Arrow
//! arrays, typed arrays.
//!
//! A batch `B` is a `u32` row count, then per column: a `u8` (1: a
//! validity bitmap follows, 0: no NULLs), the bitmap (a bit per row, least
//! significant first, set for a value), and the column's data: a fixed
//! width's values back to back, or a variable width's `u32` offsets (one
//! more than the rows) and the bytes they point into.  Every buffer starts
//! at a multiple of 8 from the start of the body (zero padding before it),
//! so it can be read in place.  A NULL's slot is zeros, or empty.

use nucleus_sql::{DataType, Error, Row, Value};

/// Rows per batch when the client asked for no batch size.
pub const DEFAULT_BATCH: usize = 16_384;

/// How a column of a type is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Values of this many bytes each.
    Fixed(usize),
    /// `u32` offsets and bytes.
    Var,
}

fn layout(t: DataType) -> Layout {
    match t {
        DataType::Bool => Layout::Fixed(1),
        DataType::SmallInt => Layout::Fixed(2),
        DataType::Integer | DataType::Real | DataType::Date => Layout::Fixed(4),
        DataType::BigInt
        | DataType::Float
        | DataType::Timestamp
        | DataType::TimestampTz
        | DataType::Time => Layout::Fixed(8),
        DataType::Interval => Layout::Fixed(16),
        DataType::Numeric => Layout::Fixed(17),
        DataType::Ext(e) if e.name() == "uuid" => Layout::Fixed(16),
        _ => Layout::Var,
    }
}

fn pad(buf: &mut Vec<u8>) {
    // Relative to the body, after the frame's 5 header bytes.
    while !(buf.len() - 5).is_multiple_of(8) {
        buf.push(0);
    }
}

fn mismatch(v: &Value, t: DataType) -> Error {
    Error {
        code: "XX000",
        message: format!("a {t} column holds {v:?}, which its columnar layout cannot"),
        hint: None,
    }
}

/// A fixed-width value's bytes.
fn fixed(buf: &mut Vec<u8>, v: &Value, t: DataType) -> Result<(), Error> {
    match (t, v) {
        (DataType::Bool, Value::Bool(b)) => buf.push(u8::from(*b)),
        (DataType::SmallInt, Value::Int(i)) => buf.extend_from_slice(&(*i as i16).to_le_bytes()),
        (DataType::Integer, Value::Int(i)) => buf.extend_from_slice(&(*i as i32).to_le_bytes()),
        (DataType::BigInt, Value::Int(i)) => buf.extend_from_slice(&i.to_le_bytes()),
        (DataType::Real, Value::Real(f)) => buf.extend_from_slice(&f.to_le_bytes()),
        (DataType::Float, Value::Float(f)) => buf.extend_from_slice(&f.to_le_bytes()),
        (DataType::Float, Value::Real(f)) => buf.extend_from_slice(&f64::from(*f).to_le_bytes()),
        (DataType::Date, Value::Date(d)) => buf.extend_from_slice(&d.to_le_bytes()),
        (DataType::Timestamp, Value::Timestamp(t))
        | (DataType::TimestampTz, Value::TimestampTz(t))
        | (DataType::Time, Value::Time(t)) => buf.extend_from_slice(&t.to_le_bytes()),
        (DataType::Interval, Value::Interval(i)) => {
            buf.extend_from_slice(&i.months.to_le_bytes());
            buf.extend_from_slice(&i.days.to_le_bytes());
            buf.extend_from_slice(&i.micros.to_le_bytes());
        }
        (DataType::Ext(_), Value::Ext(_, b)) if b.len() == 16 => buf.extend_from_slice(b),
        (DataType::Numeric, Value::Numeric(_)) => {
            // The `i128` mantissa and `u8` scale, as a row's value has them.
            let mut tagged = Vec::with_capacity(18);
            nucleus_sql::encoding::encode_value(&mut tagged, v);
            buf.extend_from_slice(&tagged[1..]);
        }
        _ => return Err(mismatch(v, t)),
    }
    Ok(())
}

/// A variable-width value's bytes: text as UTF-8 (`char(n)` and enums
/// too), `bytea` and registered types as their bytes, `inet` /
/// `cidr` as family, mask length and address, anything else (arrays) as
/// the value in the row format's encoding.
fn var(buf: &mut Vec<u8>, v: &Value, t: DataType, version: u16) {
    match v {
        Value::Text(s) => buf.extend_from_slice(s.as_bytes()),
        Value::Bytea(b) | Value::Ext(_, b) => buf.extend_from_slice(b),
        Value::Inet(n) | Value::Cidr(n) => buf.extend(n.to_bytes()),
        Value::Array(_) => crate::native::wire_value(buf, v, Some(t), version),
        v => buf.extend_from_slice(v.render().as_bytes()),
    }
}

/// The `B` message for `rows` of columns `types`.
pub fn batch(rows: &[Row], types: &[DataType], version: u16) -> Result<Vec<u8>, Error> {
    let mut buf = vec![b'B', 0, 0, 0, 0];
    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (c, &t) in types.iter().enumerate() {
        let nulls = rows.iter().any(|r| r[c].is_null());
        buf.push(u8::from(nulls));
        if nulls {
            pad(&mut buf);
            let mut bits = vec![0u8; rows.len().div_ceil(8)];
            for (i, r) in rows.iter().enumerate() {
                if !r[c].is_null() {
                    bits[i / 8] |= 1 << (i % 8);
                }
            }
            buf.extend_from_slice(&bits);
        }
        pad(&mut buf);
        match layout(t) {
            Layout::Fixed(w) => {
                for r in rows {
                    match &r[c] {
                        Value::Null => buf.resize(buf.len() + w, 0),
                        v => fixed(&mut buf, v, t)?,
                    }
                }
            }
            Layout::Var => {
                let mut data = Vec::new();
                let mut offsets = Vec::with_capacity((rows.len() + 1) * 4);
                offsets.extend_from_slice(&0u32.to_le_bytes());
                for r in rows {
                    if !r[c].is_null() {
                        var(&mut data, &r[c], t, version);
                    }
                    offsets.extend_from_slice(&(data.len() as u32).to_le_bytes());
                }
                buf.extend_from_slice(&offsets);
                pad(&mut buf);
                buf.extend_from_slice(&data);
            }
        }
        pad(&mut buf);
    }
    let len = (buf.len() - 5) as u32;
    buf[1..5].copy_from_slice(&len.to_le_bytes());
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_aligned_and_null_slots_empty() {
        let rows = vec![
            vec![Value::Int(1), Value::Text("ab".into())],
            vec![Value::Null, Value::Null],
            vec![Value::Int(-3), Value::Text("c".into())],
        ];
        let b = batch(&rows, &[DataType::Integer, DataType::Text], 7).unwrap();
        let body = &b[5..];
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 3);
        // Column 0: nulls, bitmap at 8, values at 16.
        assert_eq!(body[4], 1);
        assert_eq!(body[8], 0b101);
        let ints: Vec<i32> = body[16..28]
            .chunks(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(ints, [1, 0, -3]);
        // Column 1 at 32: nulls, bitmap at 40, offsets at 48, data at 64.
        assert_eq!(body[32], 1);
        assert_eq!(body[40], 0b101);
        let offs: Vec<u32> = body[48..64]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(offs, [0, 2, 2, 3]);
        assert_eq!(&body[64..67], b"abc");
        assert_eq!(body.len() % 8, 0);
    }
}
