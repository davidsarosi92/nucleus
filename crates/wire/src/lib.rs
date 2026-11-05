//! The nucleus protocol's wire format (`ncp`, DOCS.md §10), defined once
//! for both ends: the server (`nucleus-server`) and the Rust driver
//! (`nucleus-client`) each map their own values onto it.
//!
//! What is here: the protocol's versions, its message kinds and value
//! tags; frames (a kind, a `u32` length, the body); reading a body's
//! fields ([`Reader`]); a value's encoding ([`put`]) and decoding
//! ([`read_value`] into a [`WireValue`]); the columnar layout's widths
//! and padding ([`columnar`]).  Every integer is little-endian.  No
//! dependencies, no I/O but [`read_frame`]'s.

use std::io::{self, Read};

/// The newest protocol version.  2 adds registered types (tag 11); 3
/// tags 12–20; 4 SCRAM-SHA-256 logins; 5 a hint in `E`; 6 streaming
/// (`S`, `F`, `c`) and a fetch size in `Q`; 7 columnar results (`B`) and
/// `COPY … FROM STDIN` (`I`, `d`, `e`, `f`).
pub const VERSION: u16 = 7;
/// The oldest still spoken.
pub const MIN_VERSION: u16 = 1;
/// The largest body a frame may have.
pub const MAX_BODY: u32 = 1 << 30;

/// Message kinds (a frame's first byte).
pub mod kind {
    /// Hello (client, first).
    pub const HELLO: u8 = b'H';
    /// Authentication (server).
    pub const AUTH: u8 = b'A';
    /// Authentication response (client).
    pub const AUTH_RESPONSE: u8 = b'a';
    /// Welcome (server).
    pub const WELCOME: u8 = b'W';
    /// Query (client).
    pub const QUERY: u8 = b'Q';
    /// Columns (server).
    pub const COLUMNS: u8 = b'C';
    /// A row (server).
    pub const ROW: u8 = b'D';
    /// A columnar batch (server; client during `COPY`).
    pub const BATCH: u8 = b'B';
    /// Copy in: a `COPY … FROM STDIN` takes data (server).
    pub const COPY_IN: u8 = b'I';
    /// Copy data (client).
    pub const COPY_DATA: u8 = b'd';
    /// Copy end (client).
    pub const COPY_END: u8 = b'e';
    /// Copy fail (client).
    pub const COPY_FAIL: u8 = b'f';
    /// Done: a statement's tag and count (server).
    pub const DONE: u8 = b'K';
    /// Error (server).
    pub const ERROR: u8 = b'E';
    /// Ready, with the transaction status (server).
    pub const READY: u8 = b'R';
    /// Suspended: a batch is sent (server).
    pub const SUSPENDED: u8 = b'S';
    /// Fetch the next batch (client).
    pub const FETCH: u8 = b'F';
    /// Close: no more rows (client).
    pub const CLOSE: u8 = b'c';
    /// Bye (client).
    pub const BYE: u8 = b'X';
}

/// Value and type tags.
pub mod tag {
    /// NULL (a value) or an unknown type (a column).
    pub const NULL: u8 = 0;
    /// `boolean`: a `u8`.
    pub const BOOL: u8 = 1;
    /// `bigint` (and, before version 3, `smallint` and `integer`): `i64`.
    pub const BIGINT: u8 = 2;
    /// `double precision` (and, before version 3, `real`): `f64`.
    pub const DOUBLE: u8 = 3;
    /// `text` (and what an older client gets as text): a string.
    pub const TEXT: u8 = 4;
    /// `numeric`: `i128` mantissa, `u8` scale.
    pub const NUMERIC: u8 = 5;
    /// `date`: `i32` days since 2000-01-01.
    pub const DATE: u8 = 6;
    /// `timestamp`: `i64` µs since 2000-01-01.
    pub const TIMESTAMP: u8 = 7;
    /// `timestamptz`: `i64` µs since 2000-01-01 UTC.
    pub const TIMESTAMPTZ: u8 = 8;
    /// `interval`: `i32` months, `i32` days, `i64` µs.
    pub const INTERVAL: u8 = 9;
    /// An array: `u32` count, then the values.
    pub const ARRAY: u8 = 10;
    /// A registered type (version 2): its name, then its binary form.
    pub const EXT: u8 = 11;
    /// `smallint` (version 3): `i16`.
    pub const SMALLINT: u8 = 12;
    /// `integer` (version 3): `i32`.
    pub const INTEGER: u8 = 13;
    /// `real` (version 3): `f32`.
    pub const REAL: u8 = 14;
    /// `bytea` (version 3): `u32` length and bytes.
    pub const BYTEA: u8 = 15;
    /// `time` (version 3): `i64` µs since midnight.
    pub const TIME: u8 = 16;
    /// `char(n)` (version 3; a column type, its values text).
    pub const CHAR: u8 = 17;
    /// `inet` (version 3): family (4, 6), mask length, address.
    pub const INET: u8 = 18;
    /// `cidr` (version 3): as `inet`.
    pub const CIDR: u8 = 19;
    /// An enum (version 3; a column type with its name, its values text).
    pub const ENUM: u8 = 20;
}

/// A body too short for what it should hold, or a field not what it
/// should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Malformed {}

/// A frame: `kind`, the body's length, the body.
pub fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    put_frame(&mut out, kind, body);
    out
}

/// Appends a frame to `out`.
pub fn put_frame(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

/// Begins a frame of `kind` in `out` (its length to be set by
/// [`end_frame`]): the frame's start.
pub fn begin_frame(out: &mut Vec<u8>, kind: u8) -> usize {
    let at = out.len();
    out.extend_from_slice(&[kind, 0, 0, 0, 0]);
    at
}

/// Sets the length of the frame begun at `at`: what follows its header.
pub fn end_frame(out: &mut [u8], at: usize) {
    let len = (out.len() - at - 5) as u32;
    out[at + 1..at + 5].copy_from_slice(&len.to_le_bytes());
}

/// Reads one frame: its kind and body.
pub fn read_frame<R: Read + ?Sized>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h)?;
    let len = u32::from_le_bytes(h[1..5].try_into().unwrap());
    if len > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok((h[0], body))
}

/// Whether `buffered` (bytes received, not yet read) holds a whole frame:
/// a pipelined message waits (§10, Pipelining).
pub fn message_waiting(buffered: &[u8]) -> bool {
    buffered.len() >= 5
        && buffered.len() - 5 >= u32::from_le_bytes(buffered[1..5].try_into().unwrap()) as usize
}

/// Reads a body's fields in order.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Reads `b`.
    pub fn new(b: &'a [u8]) -> Self {
        Self { b }
    }
    /// What is left.
    pub fn rest(&self) -> &'a [u8] {
        self.b
    }
    /// Nothing is left.
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }
    /// The next byte, not taken.
    pub fn peek(&self) -> Option<u8> {
        self.b.first().copied()
    }
    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.b.len() < n {
            return Err(Malformed("message too short"));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    /// A `u8`.
    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }
    /// A `u16`.
    pub fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    /// A `u32`.
    pub fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    /// A `u64`.
    pub fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// An `i16`.
    pub fn i16(&mut self) -> Result<i16, Malformed> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    /// An `i32`.
    pub fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    /// An `i64`.
    pub fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    /// An `i128`.
    pub fn i128(&mut self) -> Result<i128, Malformed> {
        Ok(i128::from_le_bytes(self.array()?))
    }
    /// An `f32`.
    pub fn f32(&mut self) -> Result<f32, Malformed> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    /// An `f64`.
    pub fn f64(&mut self) -> Result<f64, Malformed> {
        Ok(f64::from_le_bytes(self.array()?))
    }
    /// A string's bytes: `u32` length and that many.
    pub fn bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// A string, borrowed.
    pub fn str_ref(&mut self) -> Result<&'a str, Malformed> {
        std::str::from_utf8(self.bytes()?).map_err(|_| Malformed("string is not UTF-8"))
    }
    /// A string.
    pub fn str(&mut self) -> Result<String, Malformed> {
        self.str_ref().map(str::to_string)
    }
}

/// Appending fields and values.
pub mod put {
    use super::tag;

    /// A `u16`.
    pub fn u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    /// A `u32`.
    pub fn u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    /// A `u64`.
    pub fn u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    /// Bytes with their `u32` length.
    pub fn bytes(out: &mut Vec<u8>, b: &[u8]) {
        u32(out, b.len() as u32);
        out.extend_from_slice(b);
    }
    /// A string: its `u32` length and UTF-8.
    pub fn str(out: &mut Vec<u8>, s: &str) {
        bytes(out, s.as_bytes());
    }

    /// NULL.
    pub fn null(out: &mut Vec<u8>) {
        out.push(tag::NULL);
    }
    /// A boolean.
    pub fn bool(out: &mut Vec<u8>, b: bool) {
        out.extend_from_slice(&[tag::BOOL, u8::from(b)]);
    }
    /// A `smallint`.
    pub fn smallint(out: &mut Vec<u8>, i: i16) {
        out.push(tag::SMALLINT);
        out.extend_from_slice(&i.to_le_bytes());
    }
    /// An `integer`.
    pub fn integer(out: &mut Vec<u8>, i: i32) {
        out.push(tag::INTEGER);
        out.extend_from_slice(&i.to_le_bytes());
    }
    /// A `bigint`.
    pub fn bigint(out: &mut Vec<u8>, i: i64) {
        out.push(tag::BIGINT);
        out.extend_from_slice(&i.to_le_bytes());
    }
    /// A `real`.
    pub fn real(out: &mut Vec<u8>, f: f32) {
        out.push(tag::REAL);
        out.extend_from_slice(&f.to_le_bytes());
    }
    /// A `double precision`.
    pub fn double(out: &mut Vec<u8>, f: f64) {
        out.push(tag::DOUBLE);
        out.extend_from_slice(&f.to_le_bytes());
    }
    /// Text.
    pub fn text(out: &mut Vec<u8>, s: &str) {
        out.push(tag::TEXT);
        str(out, s);
    }
    /// A `numeric`: mantissa × 10^-scale.
    pub fn numeric(out: &mut Vec<u8>, mantissa: i128, scale: u8) {
        out.push(tag::NUMERIC);
        out.extend_from_slice(&mantissa.to_le_bytes());
        out.push(scale);
    }
    /// A `date`: days since 2000-01-01.
    pub fn date(out: &mut Vec<u8>, days: i32) {
        out.push(tag::DATE);
        out.extend_from_slice(&days.to_le_bytes());
    }
    /// A `timestamp` (or with `tz`, a `timestamptz`): µs since 2000-01-01.
    pub fn timestamp(out: &mut Vec<u8>, micros: i64, tz: bool) {
        out.push(if tz { tag::TIMESTAMPTZ } else { tag::TIMESTAMP });
        out.extend_from_slice(&micros.to_le_bytes());
    }
    /// An `interval`.
    pub fn interval(out: &mut Vec<u8>, months: i32, days: i32, micros: i64) {
        out.push(tag::INTERVAL);
        out.extend_from_slice(&months.to_le_bytes());
        out.extend_from_slice(&days.to_le_bytes());
        out.extend_from_slice(&micros.to_le_bytes());
    }
    /// An array's head: its `count` values follow.
    pub fn array(out: &mut Vec<u8>, count: usize) {
        out.push(tag::ARRAY);
        u32(out, count as u32);
    }
    /// A registered type's value: its type's name and binary form.
    pub fn ext(out: &mut Vec<u8>, type_name: &str, binary: &[u8]) {
        out.push(tag::EXT);
        str(out, type_name);
        bytes(out, binary);
    }
    /// A `bytea`.
    pub fn bytea(out: &mut Vec<u8>, b: &[u8]) {
        out.push(tag::BYTEA);
        bytes(out, b);
    }
    /// A `time`: µs since midnight.
    pub fn time(out: &mut Vec<u8>, micros: i64) {
        out.push(tag::TIME);
        out.extend_from_slice(&micros.to_le_bytes());
    }
    /// An `inet` (or with `cidr`, a `cidr`): family 4 or 6, mask length,
    /// the address's 4 or 16 bytes.
    pub fn net(out: &mut Vec<u8>, cidr: bool, family: u8, bits: u8, addr: &[u8]) {
        out.push(if cidr { tag::CIDR } else { tag::INET });
        out.extend_from_slice(&[family, bits]);
        out.extend_from_slice(addr);
    }
}

/// A value as the protocol carries it, decoded ([`read_value`]); each end
/// maps it onto its own values.
#[derive(Clone, Debug, PartialEq)]
pub enum WireValue {
    /// NULL.
    Null,
    /// `boolean`.
    Bool(bool),
    /// `smallint`.
    SmallInt(i16),
    /// `integer`.
    Integer(i32),
    /// `bigint`.
    BigInt(i64),
    /// `real`.
    Real(f32),
    /// `double precision`.
    Double(f64),
    /// Text.
    Text(String),
    /// `numeric`: mantissa × 10^-scale.
    Numeric {
        /// The digits.
        mantissa: i128,
        /// Where the point is.
        scale: u8,
    },
    /// `date`: days since 2000-01-01.
    Date(i32),
    /// `timestamp`: µs since 2000-01-01.
    Timestamp(i64),
    /// `timestamptz`: µs since 2000-01-01 UTC.
    TimestampTz(i64),
    /// `interval`.
    Interval {
        /// Months.
        months: i32,
        /// Days.
        days: i32,
        /// Microseconds.
        micros: i64,
    },
    /// An array.
    Array(Vec<WireValue>),
    /// A registered type's value.
    Ext {
        /// Its type's name.
        type_name: String,
        /// Its binary form.
        bytes: Vec<u8>,
    },
    /// `bytea`.
    Bytea(Vec<u8>),
    /// `time`: µs since midnight.
    Time(i64),
    /// `inet` or (`cidr`) `cidr`.
    Net {
        /// A `cidr`, not an `inet`.
        cidr: bool,
        /// 4 or 6.
        family: u8,
        /// The mask's length.
        bits: u8,
        /// The address (4 bytes used for family 4).
        addr: [u8; 16],
    },
}

/// Reads a value (tag and payload).
pub fn read_value(r: &mut Reader) -> Result<WireValue, Malformed> {
    Ok(match r.u8()? {
        tag::NULL => WireValue::Null,
        tag::BOOL => WireValue::Bool(r.u8()? != 0),
        tag::BIGINT => WireValue::BigInt(r.i64()?),
        tag::DOUBLE => WireValue::Double(r.f64()?),
        tag::TEXT => WireValue::Text(r.str()?),
        tag::NUMERIC => WireValue::Numeric {
            mantissa: r.i128()?,
            scale: r.u8()?,
        },
        tag::DATE => WireValue::Date(r.i32()?),
        tag::TIMESTAMP => WireValue::Timestamp(r.i64()?),
        tag::TIMESTAMPTZ => WireValue::TimestampTz(r.i64()?),
        tag::INTERVAL => WireValue::Interval {
            months: r.i32()?,
            days: r.i32()?,
            micros: r.i64()?,
        },
        tag::ARRAY => {
            let n = r.u32()? as usize;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                items.push(read_value(r)?);
            }
            WireValue::Array(items)
        }
        tag::EXT => WireValue::Ext {
            type_name: r.str()?,
            bytes: r.bytes()?.to_vec(),
        },
        tag::SMALLINT => WireValue::SmallInt(r.i16()?),
        tag::INTEGER => WireValue::Integer(r.i32()?),
        tag::REAL => WireValue::Real(r.f32()?),
        tag::BYTEA => WireValue::Bytea(r.bytes()?.to_vec()),
        tag::TIME => WireValue::Time(r.i64()?),
        t @ (tag::INET | tag::CIDR) => {
            let (family, bits) = (r.u8()?, r.u8()?);
            let n = match family {
                4 => 4,
                6 => 16,
                _ => return Err(Malformed("unknown address family")),
            };
            let mut addr = [0u8; 16];
            addr[..n].copy_from_slice(r.take(n)?);
            WireValue::Net {
                cidr: t == tag::CIDR,
                family,
                bits,
                addr,
            }
        }
        _ => return Err(Malformed("unknown value tag")),
    })
}

/// Appends `v` ([`read_value`]'s inverse).
pub fn put_value(out: &mut Vec<u8>, v: &WireValue) {
    match v {
        WireValue::Null => put::null(out),
        WireValue::Bool(b) => put::bool(out, *b),
        WireValue::SmallInt(i) => put::smallint(out, *i),
        WireValue::Integer(i) => put::integer(out, *i),
        WireValue::BigInt(i) => put::bigint(out, *i),
        WireValue::Real(f) => put::real(out, *f),
        WireValue::Double(f) => put::double(out, *f),
        WireValue::Text(s) => put::text(out, s),
        WireValue::Numeric { mantissa, scale } => put::numeric(out, *mantissa, *scale),
        WireValue::Date(d) => put::date(out, *d),
        WireValue::Timestamp(t) => put::timestamp(out, *t, false),
        WireValue::TimestampTz(t) => put::timestamp(out, *t, true),
        WireValue::Interval {
            months,
            days,
            micros,
        } => put::interval(out, *months, *days, *micros),
        WireValue::Array(items) => {
            put::array(out, items.len());
            for i in items {
                put_value(out, i);
            }
        }
        WireValue::Ext { type_name, bytes } => put::ext(out, type_name, bytes),
        WireValue::Bytea(b) => put::bytea(out, b),
        WireValue::Time(t) => put::time(out, *t),
        WireValue::Net {
            cidr,
            family,
            bits,
            addr,
        } => put::net(out, *cidr, *family, *bits, &addr[..net_len(*family)]),
    }
}

/// An address's length for its family.
pub fn net_len(family: u8) -> usize {
    if family == 4 { 4 } else { 16 }
}

/// The columnar layout (version 7, §10 *Columnar results*): a batch's
/// body is a `u32` row count, then per column a `u8` (1: a validity
/// bitmap follows), the bitmap, and the data — every bitmap, data and
/// offsets buffer at a multiple of 8 bytes from the body's start.
pub mod columnar {
    use super::tag;

    /// The bytes a value of a column with type tag `t` (a registered
    /// type's: named `ext`) takes in a batch; 0: variable width (`u32`
    /// offsets, then the bytes).
    pub fn width(t: u8, ext: Option<&str>) -> usize {
        match t {
            tag::BOOL => 1,
            tag::SMALLINT => 2,
            tag::INTEGER | tag::REAL | tag::DATE => 4,
            tag::BIGINT | tag::DOUBLE | tag::TIMESTAMP | tag::TIMESTAMPTZ | tag::TIME => 8,
            tag::INTERVAL => 16,
            tag::NUMERIC => 17,
            tag::EXT if ext == Some("uuid") => 16,
            _ => 0,
        }
    }

    /// Pads `buf` with zeros to a multiple of 8 bytes from `start` (where
    /// the body begins in it).
    pub fn pad(buf: &mut Vec<u8>, start: usize) {
        let n = (buf.len() - start).next_multiple_of(8) + start;
        buf.resize(n, 0);
    }

    /// `at` rounded up to a multiple of 8.
    pub fn align(at: usize) -> usize {
        at.next_multiple_of(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_back_as_written() {
        let values = vec![
            WireValue::Null,
            WireValue::Bool(true),
            WireValue::SmallInt(-3),
            WireValue::Integer(1 << 20),
            WireValue::BigInt(-(1 << 40)),
            WireValue::Real(0.5),
            WireValue::Double(-2.25),
            WireValue::Text("héllo".into()),
            WireValue::Numeric {
                mantissa: -12345,
                scale: 2,
            },
            WireValue::Date(8766),
            WireValue::Timestamp(1),
            WireValue::TimestampTz(-1),
            WireValue::Interval {
                months: 1,
                days: 2,
                micros: 3,
            },
            WireValue::Array(vec![WireValue::Text("a".into()), WireValue::Null]),
            WireValue::Ext {
                type_name: "uuid".into(),
                bytes: vec![7; 16],
            },
            WireValue::Bytea(vec![0, 255]),
            WireValue::Time(86_400_000_000),
            WireValue::Net {
                cidr: false,
                family: 4,
                bits: 24,
                addr: [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            },
            WireValue::Net {
                cidr: true,
                family: 6,
                bits: 64,
                addr: [0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            },
        ];
        let mut out = Vec::new();
        for v in &values {
            put_value(&mut out, v);
        }
        let mut r = Reader::new(&out);
        for v in &values {
            assert_eq!(&read_value(&mut r).unwrap(), v);
        }
        assert!(r.is_empty());
        // Cut anywhere, it is malformed, never a panic.
        for n in 0..out.len() {
            let mut r = Reader::new(&out[..n]);
            while !r.is_empty() {
                if read_value(&mut r).is_err() {
                    break;
                }
            }
        }
    }

    #[test]
    fn frames_and_waiting_messages() {
        let f = frame(kind::QUERY, b"abc");
        assert_eq!(f, [b'Q', 3, 0, 0, 0, b'a', b'b', b'c']);
        assert!(message_waiting(&f));
        assert!(!message_waiting(&f[..7]));
        assert!(!message_waiting(&f[..4]));
        let (k, body) = read_frame(&mut &f[..]).unwrap();
        assert_eq!((k, body.as_slice()), (b'Q', &b"abc"[..]));
        let mut out = vec![9];
        let at = begin_frame(&mut out, kind::READY);
        out.push(0);
        end_frame(&mut out, at);
        assert_eq!(out, [9, b'R', 1, 0, 0, 0, 0]);
        let mut big = vec![b'Q'];
        big.extend_from_slice(&(MAX_BODY + 1).to_le_bytes());
        assert!(read_frame(&mut &big[..]).is_err());
    }

    #[test]
    fn columnar_padding() {
        let mut b = vec![1, 2, 3, 4, 5, 6, 7];
        columnar::pad(&mut b, 5);
        assert_eq!(b.len(), 13);
        columnar::pad(&mut b, 5);
        assert_eq!(b.len(), 13);
        assert_eq!(columnar::align(9), 16);
        assert_eq!(columnar::width(tag::NUMERIC, None), 17);
        assert_eq!(columnar::width(tag::EXT, Some("uuid")), 16);
        assert_eq!(columnar::width(tag::TEXT, None), 0);
    }
}
