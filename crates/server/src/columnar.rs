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

use nucleus_sql::vector::{Batch, TimeKind, Vector};
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

/// A column of a batch being built.
#[derive(Default)]
struct Col {
    /// A bit per row, set for a value.
    valid: Vec<u8>,
    nulls: bool,
    /// Fixed: the values; variable: the bytes.
    data: Vec<u8>,
    /// Variable: the offsets after each row (`u32`s).
    offsets: Vec<u8>,
}

impl Col {
    fn mark(&mut self, i: usize, value: bool) {
        if i.is_multiple_of(8) {
            self.valid.push(0);
        }
        if value {
            self.valid[i / 8] |= 1 << (i % 8);
        } else {
            self.nulls = true;
        }
    }
}

/// A `B` message built a row or a batch of columns at a time: the same
/// bytes either way, a batch's typed vectors copied without a value per
/// cell.
pub struct Builder {
    types: Vec<DataType>,
    version: u16,
    rows: usize,
    cols: Vec<Col>,
}

impl Builder {
    /// A batch of columns `types`.
    pub fn new(types: &[DataType], version: u16) -> Self {
        Self {
            types: types.to_vec(),
            version,
            rows: 0,
            cols: types.iter().map(|_| Col::default()).collect(),
        }
    }

    /// Rows so far.
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    fn value(&mut self, c: usize, v: &Value) -> Result<(), Error> {
        let t = self.types[c];
        let i = self.rows;
        let col = &mut self.cols[c];
        col.mark(i, !v.is_null());
        match layout(t) {
            Layout::Fixed(w) => match v {
                Value::Null => col.data.resize(col.data.len() + w, 0),
                v => fixed(&mut col.data, v, t)?,
            },
            Layout::Var => {
                if !v.is_null() {
                    var(&mut col.data, v, t, self.version);
                }
                col.offsets
                    .extend_from_slice(&(col.data.len() as u32).to_le_bytes());
            }
        }
        Ok(())
    }

    /// Adds a row.
    pub fn push_row(&mut self, row: &Row) -> Result<(), Error> {
        for (c, v) in row.iter().enumerate().take(self.types.len()) {
            self.value(c, v)?;
        }
        self.rows += 1;
        Ok(())
    }

    /// Adds the rows in play of `b` from the `from`-th to before the
    /// `to`-th.
    pub fn push_batch(&mut self, b: &Batch, from: usize, to: usize) -> Result<(), Error> {
        let rows: Vec<usize> = (from..to).map(|k| b.sel.nth(k)).collect();
        for (c, v) in b.cols.iter().enumerate().take(self.types.len()) {
            let t = self.types[c];
            let start = self.rows;
            let col = &mut self.cols[c];
            let null = |n: &Option<Vec<bool>>, i: usize| n.as_ref().is_some_and(|n| n[i]);
            match (layout(t), &**v) {
                (Layout::Fixed(w @ (2 | 4 | 8)), Vector::Int(d, n))
                    if matches!(t, DataType::SmallInt | DataType::Integer | DataType::BigInt) =>
                {
                    for (k, &i) in rows.iter().enumerate() {
                        col.mark(start + k, !null(n, i));
                        let x = if null(n, i) { 0 } else { d[i] };
                        col.data.extend_from_slice(&x.to_le_bytes()[..w]);
                    }
                }
                (Layout::Fixed(8), Vector::Float(d, n)) if t == DataType::Float => {
                    for (k, &i) in rows.iter().enumerate() {
                        col.mark(start + k, !null(n, i));
                        let x = if null(n, i) { 0.0 } else { d[i] };
                        col.data.extend_from_slice(&x.to_le_bytes());
                    }
                }
                (Layout::Fixed(1), Vector::Bool(d, n)) if t == DataType::Bool => {
                    for (k, &i) in rows.iter().enumerate() {
                        col.mark(start + k, !null(n, i));
                        col.data.push(u8::from(!null(n, i) && d[i]));
                    }
                }
                (Layout::Fixed(w @ (4 | 8)), Vector::Time(kind, d, n))
                    if matches!(
                        (kind, t),
                        (TimeKind::Date, DataType::Date)
                            | (TimeKind::Time, DataType::Time)
                            | (TimeKind::Timestamp, DataType::Timestamp)
                            | (TimeKind::TimestampTz, DataType::TimestampTz)
                    ) =>
                {
                    for (k, &i) in rows.iter().enumerate() {
                        col.mark(start + k, !null(n, i));
                        let x = if null(n, i) { 0 } else { d[i] };
                        col.data.extend_from_slice(&x.to_le_bytes()[..w]);
                    }
                }
                // A text value's bytes, as `var` sends them.
                (Layout::Var, Vector::Text(o, bytes, n)) => {
                    for (k, &i) in rows.iter().enumerate() {
                        col.mark(start + k, !null(n, i));
                        if !null(n, i) {
                            col.data
                                .extend_from_slice(&bytes[o[i] as usize..o[i + 1] as usize]);
                        }
                        col.offsets
                            .extend_from_slice(&(col.data.len() as u32).to_le_bytes());
                    }
                }
                (_, v) => {
                    // Anything else value by value.
                    for (k, &i) in rows.iter().enumerate() {
                        self.rows = start + k;
                        self.value(c, &v.value(i))?;
                    }
                    self.rows = start;
                }
            }
        }
        self.rows += rows.len();
        Ok(())
    }

    /// The `B` message of the rows so far; the builder is empty after.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut buf = vec![b'B', 0, 0, 0, 0];
        buf.extend_from_slice(&(self.rows as u32).to_le_bytes());
        for (col, &t) in self.cols.iter_mut().zip(&self.types) {
            let col = std::mem::take(col);
            buf.push(u8::from(col.nulls));
            if col.nulls {
                pad(&mut buf);
                buf.extend_from_slice(&col.valid);
            }
            pad(&mut buf);
            match layout(t) {
                Layout::Fixed(_) => buf.extend_from_slice(&col.data),
                Layout::Var => {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                    buf.extend_from_slice(&col.offsets);
                    pad(&mut buf);
                    buf.extend_from_slice(&col.data);
                }
            }
            pad(&mut buf);
        }
        self.rows = 0;
        let len = (buf.len() - 5) as u32;
        buf[1..5].copy_from_slice(&len.to_le_bytes());
        buf
    }
}

/// The `B` message for `rows` of columns `types`.
#[cfg(test)]
fn batch(rows: &[Row], types: &[DataType], version: u16) -> Result<Vec<u8>, Error> {
    let mut b = Builder::new(types, version);
    for r in rows {
        b.push_row(r)?;
    }
    Ok(b.finish())
}

/// The rows of a client's batch `body` (`COPY … FROM STDIN`) of columns
/// `types`: each slot read as the parameter of its type it encodes.
pub fn rows(body: &[u8], types: &[DataType]) -> Result<Vec<Row>, Error> {
    let bad = || Error {
        code: "08P01",
        message: "malformed batch".into(),
        hint: None,
    };
    let take = |at: &mut usize, n: usize| -> Result<std::ops::Range<usize>, Error> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= body.len())
            .ok_or_else(bad)?;
        let r = *at..end;
        *at = end;
        Ok(r)
    };
    let align = |at: &mut usize| *at = at.div_ceil(8) * 8;
    let mut at = 0;
    let n = u32::from_le_bytes(body[take(&mut at, 4)?].try_into().unwrap()) as usize;
    let mut rows: Vec<Row> = (0..n).map(|_| Vec::with_capacity(types.len())).collect();
    for &t in types {
        let nulls = body[take(&mut at, 1)?][0] != 0;
        let valid = if nulls {
            align(&mut at);
            Some(take(&mut at, n.div_ceil(8))?)
        } else {
            None
        };
        align(&mut at);
        let is_null = |i: usize| {
            valid
                .as_ref()
                .is_some_and(|v| body[v.start + i / 8] & (1 << (i % 8)) == 0)
        };
        // A slot as the parameter it encodes: the type's tag before it.
        let decode = |tagged: &[u8]| -> Result<Value, Error> {
            crate::native::parameter(&mut nucleus_sql::encoding::Reader::new(tagged))
        };
        match layout(t) {
            Layout::Fixed(w) => {
                let data = take(&mut at, n * w)?;
                for (i, row) in rows.iter_mut().enumerate() {
                    if is_null(i) {
                        row.push(Value::Null);
                        continue;
                    }
                    let slot = &body[data.start + i * w..data.start + (i + 1) * w];
                    let mut tagged = Vec::with_capacity(w + 8);
                    match t {
                        DataType::Ext(e) => {
                            tagged.push(11);
                            tagged.extend_from_slice(&(e.name().len() as u32).to_le_bytes());
                            tagged.extend_from_slice(e.name().as_bytes());
                            tagged.extend_from_slice(&(w as u32).to_le_bytes());
                        }
                        // A timestamp's, a date's, … tag; integers by width.
                        _ => tagged.push(fixed_tag(t)),
                    }
                    tagged.extend_from_slice(slot);
                    row.push(decode(&tagged)?);
                }
            }
            Layout::Var => {
                let offs = take(&mut at, (n + 1) * 4)?;
                let off = |i: usize| {
                    u32::from_le_bytes(
                        body[offs.start + 4 * i..offs.start + 4 * i + 4]
                            .try_into()
                            .unwrap(),
                    ) as usize
                };
                align(&mut at);
                let data = take(&mut at, off(n))?;
                for (i, row) in rows.iter_mut().enumerate() {
                    if is_null(i) {
                        row.push(Value::Null);
                        continue;
                    }
                    let (a, b) = (off(i), off(i + 1));
                    if a > b || b > data.len() {
                        return Err(bad());
                    }
                    let slot = &body[data.start + a..data.start + b];
                    row.push(match t {
                        DataType::Bytea => Value::Bytea(slot.into()),
                        DataType::Inet | DataType::Cidr => {
                            let mut tagged = vec![if t == DataType::Inet { 18 } else { 19 }];
                            tagged.extend_from_slice(slot);
                            decode(&tagged)?
                        }
                        DataType::Array(_) => decode(slot)?,
                        DataType::Ext(e) => {
                            let mut tagged = vec![11];
                            tagged.extend_from_slice(&(e.name().len() as u32).to_le_bytes());
                            tagged.extend_from_slice(e.name().as_bytes());
                            tagged.extend_from_slice(&(slot.len() as u32).to_le_bytes());
                            tagged.extend_from_slice(slot);
                            decode(&tagged)?
                        }
                        // Text (a char(n)'s or an enum's label: the column
                        // converts it).
                        _ => Value::Text(String::from_utf8(slot.to_vec()).map_err(|_| {
                            Error::invalid_text("invalid byte sequence for encoding \"UTF8\"")
                        })?),
                    });
                }
            }
        }
        align(&mut at);
    }
    Ok(rows)
}

/// The value tag a fixed-width column's slots are the payload of.
fn fixed_tag(t: DataType) -> u8 {
    match t {
        DataType::Bool => 1,
        DataType::SmallInt => 12,
        DataType::Integer => 13,
        DataType::BigInt => 2,
        DataType::Real => 14,
        DataType::Float => 3,
        DataType::Numeric => 5,
        DataType::Date => 6,
        DataType::Timestamp => 7,
        DataType::TimestampTz => 8,
        DataType::Interval => 9,
        _ => 16, // time
    }
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

    #[test]
    fn a_batch_of_columns_gives_the_bytes_of_its_rows() {
        use nucleus_sql::vector::{Batch, Sel};
        use std::rc::Rc;
        let types = [
            DataType::SmallInt,
            DataType::Integer,
            DataType::BigInt,
            DataType::Float,
            DataType::Bool,
            DataType::Text,
            DataType::Date,
            DataType::Timestamp,
            DataType::Numeric,
            DataType::Integer,
        ];
        let n = 50;
        let null = |i: usize, k: usize| (i + k).is_multiple_of(4);
        let rows: Vec<Row> = (0..n)
            .map(|i| {
                let v = |k: usize, v: Value| if null(i, k) { Value::Null } else { v };
                vec![
                    v(0, Value::Int(i as i64 - 20)),
                    v(1, Value::Int(i as i64 * 1000)),
                    v(2, Value::Int(i as i64 * 1_000_000_007)),
                    v(3, Value::Float(i as f64 / 3.0)),
                    v(4, Value::Bool(i % 3 == 0)),
                    v(5, Value::Text(format!("t{i}"))),
                    v(6, Value::Date(i as i32 - 9)),
                    v(7, Value::Timestamp(i as i64 * 60_000_000)),
                    v(
                        8,
                        Value::Numeric(nucleus_sql::decimal::Decimal::from_i64(i as i64)),
                    ),
                    Value::Int(7),
                ]
            })
            .collect();
        let mut cols: Vec<Rc<Vector>> = (0..types.len() - 1)
            .map(|c| Rc::new(Vector::typed(rows.iter().map(|r| r[c].clone()).collect())))
            .collect();
        cols.push(Rc::new(Vector::Const(Value::Int(7))));
        // Every other row in play, pushed in pieces.
        let sel: Vec<u32> = (0..n as u32).filter(|i| i % 2 == 1).collect();
        let b = Batch {
            len: n,
            cols,
            sel: Sel::Rows(sel.clone()),
        };
        let mut built = Builder::new(&types, 7);
        built.push_batch(&b, 0, 7).unwrap();
        built.push_batch(&b, 7, sel.len()).unwrap();
        let want: Vec<Row> = sel.iter().map(|&i| rows[i as usize].clone()).collect();
        assert_eq!(built.finish(), batch(&want, &types, 7).unwrap());
    }

    #[test]
    fn a_batch_reads_back_as_its_rows() {
        let types = [
            DataType::Integer,
            DataType::Text,
            DataType::Float,
            DataType::Date,
            DataType::Bool,
        ];
        let rows = vec![
            vec![
                Value::Int(1),
                Value::Text("ab".into()),
                Value::Float(0.5),
                Value::Date(3),
                Value::Bool(true),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int(-3),
                Value::Text("c".into()),
                Value::Float(-1.0),
                Value::Date(-1),
                Value::Bool(false),
            ],
        ];
        let b = batch(&rows, &types, 7).unwrap();
        assert_eq!(super::rows(&b[5..], &types).unwrap(), rows);
    }
}
