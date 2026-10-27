//! The Rust driver for nucleus (PLAN.md §M14.7–§M14.8): the nucleus
//! protocol (`ncp`, DOCS §10), client side.  No dependencies.
//!
//! ```no_run
//! use nucleus_client::{Client, Config, Value};
//!
//! let mut c = Client::connect(Config::default())?;
//! c.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)", &[])?;
//! c.execute("INSERT INTO t VALUES ($1, $2)", &[Value::Int(1), Value::from("one")])?;
//! for row in c.rows("SELECT id, name FROM t", &[])? {
//!     println!("{} {}", row[0], row[1]);
//! }
//! // A large result, a thousand rows at a time:
//! for row in c.cursor("SELECT id FROM t", &[], 1000)? {
//!     println!("{}", row?[0]);
//! }
//! // Or a column at a time: typed buffers, no value per cell.
//! for batch in c.batches("SELECT id FROM t", &[], 65536)? {
//!     if let Some(ids) = batch?.column(0).i32s() {
//!         println!("{}", ids.iter().map(|&i| i64::from(i)).sum::<i64>());
//!     }
//! }
//! # Ok::<(), nucleus_client::Error>(())
//! ```

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream};
use std::time::Duration;

/// The protocol version this driver speaks (2: registered types such as
/// `uuid` arrive in their binary form; 3: `smallint`, `integer`, `real`,
/// `bytea`, `time`, `inet` and `cidr` in theirs, `char(n)` and enums
/// named as such; 4: the password is proved by SCRAM-SHA-256, never
/// sent; 5: errors carry a hint; 6: results stream, and a [`Cursor`]
/// fetches them in batches; 7: columnar results, [`Client::batches`]).
pub const PROTOCOL_VERSION: u16 = 7;
/// A [`Cursor`]'s batch when none is given: rows the server sends before
/// it waits for the client.
pub const DEFAULT_FETCH: u32 = 1000;
/// The nucleus protocol's default port.
pub const DEFAULT_PORT: u16 = 20_035;

/// Where and as whom to connect.
#[derive(Clone, Debug)]
pub struct Config {
    /// Host name or address.
    pub host: String,
    /// Port.
    pub port: u16,
    /// User name.
    pub user: String,
    /// Database name.
    pub database: String,
    /// Password (`None` sends an empty one).
    pub password: Option<String>,
    /// Sent to the server (it may log it).
    pub client_name: String,
    /// How long to wait for the connection.
    pub connect_timeout: Duration,
    /// TLS (`None`: none) — a module's ([`Tls`]; `nucleus-tls`).
    pub tls: Option<std::sync::Arc<dyn Tls>>,
    /// A bearer token (an OpenID Connect identity token) to log in with
    /// instead of the password: it names the user (OAUTHBEARER).
    pub token: Option<String>,
}

/// A byte stream to the server.
pub trait Stream: Read + Write + Send {}

impl<T: Read + Write + Send> Stream for T {}

/// The client side of TLS: a module provides it (`nucleus-tls`), so this
/// crate carries no TLS library.  The client starts the handshake as soon
/// as it is connected; the server sees it and answers in kind (DOCS §10).
pub trait Tls: Send + Sync + fmt::Debug {
    /// Runs a handshake on `tcp` as the client, checking the server is
    /// `host` as the implementation requires.
    fn connect(&self, tcp: TcpStream, host: &str) -> io::Result<Secured>;
}

/// An encrypted stream.
pub struct Secured {
    /// The stream.
    pub stream: Box<dyn Stream>,
    /// The session's `tls-server-end-point` value (RFC 5929), which binds
    /// SCRAM to it (SCRAM-SHA-256-PLUS); `None` if the server's certificate
    /// defines none.
    pub channel_binding: Option<Vec<u8>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            user: "nucleus".into(),
            database: "nucleus".into(),
            // The server's default credentials.
            password: Some("nucleus".into()),
            client_name: "nucleus-client (rust)".into(),
            connect_timeout: Duration::from_secs(10),
            tls: None,
            token: None,
        }
    }
}

/// An error the server reported: SQLSTATE, message and perhaps a hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError {
    /// SQLSTATE (5 characters).
    pub code: String,
    /// The message.
    pub message: String,
    /// What to do about it — the name meant, the cast that would work
    /// (protocol version 5; `None` from older servers).
    pub hint: Option<String>,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// An error.
#[derive(Debug)]
pub enum Error {
    /// The network.
    Io(io::Error),
    /// The server reported an error.
    Server(ServerError),
    /// Something this driver does not understand.
    Protocol(String),
    /// A value that cannot be sent as its column's type (`copy_rows`).
    Value(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Server(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Value(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// A column type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    /// Not known (values come as text).
    Unknown,
    /// `boolean`.
    Bool,
    /// `smallint`.
    SmallInt,
    /// `integer`.
    Integer,
    /// `bigint`.
    Int,
    /// `real`.
    Real,
    /// `double precision`.
    Float,
    /// `text`.
    Text,
    /// `char(n)` (its values are text, blank-padded).
    Char,
    /// `bytea`.
    Bytea,
    /// `time` (of day).
    Time,
    /// `inet`.
    Inet,
    /// `cidr`.
    Cidr,
    /// An enum, by name (its values are its labels, as text).
    Enum(String),
    /// `numeric`.
    Numeric,
    /// `date`.
    Date,
    /// `timestamp`.
    Timestamp,
    /// `timestamptz`.
    TimestampTz,
    /// `interval`.
    Interval,
    /// An array of the element type.
    Array(Box<Type>),
    /// `uuid`.
    Uuid,
    /// Another registered type, by name.
    Other(String),
}

fn type_of_tag(t: u8) -> Type {
    match t {
        1 => Type::Bool,
        2 => Type::Int,
        3 => Type::Float,
        4 => Type::Text,
        5 => Type::Numeric,
        6 => Type::Date,
        7 => Type::Timestamp,
        8 => Type::TimestampTz,
        9 => Type::Interval,
        12 => Type::SmallInt,
        13 => Type::Integer,
        14 => Type::Real,
        15 => Type::Bytea,
        16 => Type::Time,
        17 => Type::Char,
        18 => Type::Inet,
        19 => Type::Cidr,
        _ => Type::Unknown,
    }
}

/// A value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// NULL.
    Null,
    /// A boolean.
    Bool(bool),
    /// A `smallint`.
    SmallInt(i16),
    /// An `integer`.
    Integer(i32),
    /// A 64-bit integer (`bigint`; any integer as a parameter).
    Int(i64),
    /// A `real`.
    Real(f32),
    /// A double.
    Float(f64),
    /// Text.
    Text(String),
    /// A `bytea`.
    Bytes(Vec<u8>),
    /// A `time`: microseconds since midnight (`24:00:00` included).
    Time(i64),
    /// An `inet`: an address and its mask length.
    Inet {
        /// The address.
        addr: IpAddr,
        /// The mask length (32 or 128: a host).
        bits: u8,
    },
    /// A `cidr`: a network.
    Cidr {
        /// The network address.
        addr: IpAddr,
        /// The mask length.
        bits: u8,
    },
    /// An exact decimal: `mantissa × 10^-scale`.
    Numeric {
        /// The unscaled value.
        mantissa: i128,
        /// Digits after the point.
        scale: u8,
    },
    /// Days since 2000-01-01.
    Date(i32),
    /// Microseconds since 2000-01-01 00:00.
    Timestamp(i64),
    /// Microseconds since 2000-01-01 00:00 UTC.
    TimestampTz(i64),
    /// An interval.
    Interval {
        /// Months.
        months: i32,
        /// Days.
        days: i32,
        /// Microseconds.
        micros: i64,
    },
    /// An array.
    Array(Vec<Value>),
    /// A `uuid`: its 16 bytes.
    Uuid([u8; 16]),
    /// A value of another registered type: its name and binary form.
    Other {
        /// The type's name.
        type_name: String,
        /// The value's binary (canonical) form.
        bytes: Vec<u8>,
    },
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Integer(i)
    }
}

impl From<i16> for Value {
    fn from(i: i16) -> Self {
        Value::SmallInt(i)
    }
}

impl From<f32> for Value {
    fn from(f: f32) -> Self {
        Value::Real(f)
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(b)
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::Bytes(b.to_vec())
    }
}

impl From<IpAddr> for Value {
    /// A host address (`inet` with a full mask).
    fn from(addr: IpAddr) -> Self {
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        Value::Inet { addr, bits }
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

impl Value {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(0),
            Value::Bool(b) => out.extend_from_slice(&[1, u8::from(*b)]),
            Value::SmallInt(i) => {
                out.push(12);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Integer(i) => {
                out.push(13);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Int(i) => {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Real(f) => {
                out.push(14);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(3);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Bytes(b) => {
                out.push(15);
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
            Value::Time(t) => {
                out.push(16);
                out.extend_from_slice(&t.to_le_bytes());
            }
            Value::Inet { addr, bits } | Value::Cidr { addr, bits } => {
                out.push(if matches!(self, Value::Inet { .. }) {
                    18
                } else {
                    19
                });
                match addr {
                    IpAddr::V4(a) => {
                        out.extend_from_slice(&[4, *bits]);
                        out.extend_from_slice(&a.octets());
                    }
                    IpAddr::V6(a) => {
                        out.extend_from_slice(&[6, *bits]);
                        out.extend_from_slice(&a.octets());
                    }
                }
            }
            Value::Text(s) => {
                out.push(4);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Numeric { mantissa, scale } => {
                out.push(5);
                out.extend_from_slice(&mantissa.to_le_bytes());
                out.push(*scale);
            }
            Value::Date(d) => {
                out.push(6);
                out.extend_from_slice(&d.to_le_bytes());
            }
            Value::Timestamp(t) | Value::TimestampTz(t) => {
                out.push(if matches!(self, Value::Timestamp(_)) {
                    7
                } else {
                    8
                });
                out.extend_from_slice(&t.to_le_bytes());
            }
            Value::Interval {
                months,
                days,
                micros,
            } => {
                out.push(9);
                out.extend_from_slice(&months.to_le_bytes());
                out.extend_from_slice(&days.to_le_bytes());
                out.extend_from_slice(&micros.to_le_bytes());
            }
            Value::Array(items) => {
                out.push(10);
                out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for v in items {
                    v.encode(out);
                }
            }
            Value::Uuid(b) => encode_ext(out, "uuid", b),
            Value::Other { type_name, bytes } => encode_ext(out, type_name, bytes),
        }
    }

    fn decode(r: &mut Input) -> Result<Value, Error> {
        Ok(match r.u8()? {
            0 => Value::Null,
            1 => Value::Bool(r.u8()? != 0),
            2 => Value::Int(r.i64()?),
            12 => Value::SmallInt(i16::from_le_bytes(r.take(2)?.try_into().unwrap())),
            13 => Value::Integer(r.i32()?),
            14 => Value::Real(f32::from_bits(r.u32()?)),
            15 => {
                let n = r.u32()? as usize;
                Value::Bytes(r.take(n)?.to_vec())
            }
            16 => Value::Time(r.i64()?),
            tag @ (18 | 19) => {
                let (family, bits) = (r.u8()?, r.u8()?);
                let addr = match family {
                    4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?).unwrap())),
                    6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?).unwrap())),
                    f => return Err(Error::Protocol(format!("unknown address family {f}"))),
                };
                if tag == 18 {
                    Value::Inet { addr, bits }
                } else {
                    Value::Cidr { addr, bits }
                }
            }
            3 => Value::Float(f64::from_bits(r.i64()? as u64)),
            4 => Value::Text(r.str()?),
            5 => {
                let m = i128::from_le_bytes(r.take(16)?.try_into().unwrap());
                Value::Numeric {
                    mantissa: m,
                    scale: r.u8()?,
                }
            }
            6 => Value::Date(r.i32()?),
            7 => Value::Timestamp(r.i64()?),
            8 => Value::TimestampTz(r.i64()?),
            9 => Value::Interval {
                months: r.i32()?,
                days: r.i32()?,
                micros: r.i64()?,
            },
            10 => {
                let n = r.u32()? as usize;
                let mut items = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    items.push(Value::decode(r)?);
                }
                Value::Array(items)
            }
            11 => {
                let type_name = r.str()?;
                let n = r.u32()? as usize;
                let bytes = r.take(n)?;
                match (type_name.as_str(), <[u8; 16]>::try_from(bytes)) {
                    ("uuid", Ok(b)) => Value::Uuid(b),
                    _ => Value::Other {
                        type_name,
                        bytes: bytes.to_vec(),
                    },
                }
            }
            t => return Err(Error::Protocol(format!("unknown value tag {t}"))),
        })
    }

    /// Whether this is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// A registered type's value: tag 11, its name, its binary form.
fn encode_ext(out: &mut Vec<u8>, name: &str, bytes: &[u8]) {
    out.push(11);
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The canonical text of a uuid's 16 bytes (`8-4-4-4-12`, lower case).
pub fn uuid_text(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// `(year, month, day)` of a day number since 2000-01-01 (proleptic
/// Gregorian; year 0 is 1 BC).
pub fn date_parts(days: i32) -> (i64, u32, u32) {
    let z = i64::from(days) + 10_957 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fmt_ts(f: &mut fmt::Formatter<'_>, us: i64, tz: bool) -> fmt::Result {
    const DAY: i64 = 86_400_000_000;
    let (y, m, d) = date_parts(us.div_euclid(DAY) as i32);
    let t = us.rem_euclid(DAY);
    let (h, mi, s, frac) = (
        t / 3_600_000_000,
        t / 60_000_000 % 60,
        t / 1_000_000 % 60,
        t % 1_000_000,
    );
    write!(f, "{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")?;
    if frac != 0 {
        write!(f, ".{}", format!("{frac:06}").trim_end_matches('0'))?;
    }
    if tz {
        write!(f, "+00")?;
    }
    Ok(())
}

impl fmt::Display for Value {
    /// As nucleus prints values (NULL as an empty string).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Bool(b) => f.write_str(if *b { "t" } else { "f" }),
            Value::SmallInt(i) => write!(f, "{i}"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) => f.write_str(s),
            Value::Bytes(b) => {
                f.write_str("\\x")?;
                b.iter().try_for_each(|b| write!(f, "{b:02x}"))
            }
            Value::Time(t) => {
                let (h, m, s, us) = (
                    t / 3_600_000_000,
                    t / 60_000_000 % 60,
                    t / 1_000_000 % 60,
                    t % 1_000_000,
                );
                write!(f, "{h:02}:{m:02}:{s:02}")?;
                if us != 0 {
                    write!(f, ".{}", format!("{us:06}").trim_end_matches('0'))?;
                }
                Ok(())
            }
            Value::Inet { addr, bits } => {
                let full = if addr.is_ipv4() { 32 } else { 128 };
                if *bits == full {
                    write!(f, "{addr}")
                } else {
                    write!(f, "{addr}/{bits}")
                }
            }
            Value::Cidr { addr, bits } => write!(f, "{addr}/{bits}"),
            Value::Numeric { mantissa, scale } => {
                let digits = mantissa.unsigned_abs().to_string();
                let s = usize::from(*scale);
                let sign = if *mantissa < 0 { "-" } else { "" };
                if s == 0 {
                    return write!(f, "{sign}{digits}");
                }
                let padded = format!("{digits:0>width$}", width = s + 1);
                let (int, frac) = padded.split_at(padded.len() - s);
                write!(f, "{sign}{int}.{frac}")
            }
            Value::Date(d) => {
                let (y, m, day) = date_parts(*d);
                write!(f, "{y:04}-{m:02}-{day:02}")
            }
            Value::Timestamp(t) => fmt_ts(f, *t, false),
            Value::TimestampTz(t) => fmt_ts(f, *t, true),
            Value::Interval {
                months,
                days,
                micros,
            } => write!(f, "{months} mons {days} days {micros} us"),
            Value::Uuid(b) => f.write_str(&uuid_text(b)),
            Value::Other { bytes, .. } => {
                f.write_str("\\x")?;
                bytes.iter().try_for_each(|b| write!(f, "{b:02x}"))
            }
            Value::Array(items) => {
                f.write_str("{")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    if v.is_null() {
                        f.write_str("NULL")?;
                    } else {
                        write!(f, "{v}")?;
                    }
                }
                f.write_str("}")
            }
        }
    }
}

/// A result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Name.
    pub name: String,
    /// Type.
    pub ty: Type,
}

/// One statement's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// The columns (empty for a statement without rows).
    pub columns: Vec<Column>,
    /// The rows.
    pub rows: Vec<Vec<Value>>,
    /// `SELECT`, `INSERT`, `CREATE TABLE`, …
    pub command: String,
    /// Rows returned or affected.
    pub count: u64,
    /// Whether the statement returns rows (even zero).
    pub has_rows: bool,
}

/// How results come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Typed values.
    Typed,
    /// Every value as text, as `ncsql` prints it.
    Text,
}

/// The transaction status after the last query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// Not in a transaction block.
    Idle,
    /// In a transaction block.
    InBlock,
    /// In a failed transaction block.
    Failed,
}

struct Input<'a> {
    b: &'a [u8],
}

impl<'a> Input<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.b.len() < n {
            return Err(Error::Protocol("message too short".into()));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    /// The `E` message's fields: the hint, from version 5, follows.
    fn server_error(&mut self) -> Result<ServerError, Error> {
        let code = self.str()?;
        let message = self.str()?;
        let hint = if self.b.is_empty() {
            None
        } else {
            Some(self.str()?).filter(|h| !h.is_empty())
        };
        Ok(ServerError {
            code,
            message,
            hint,
        })
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, Error> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String, Error> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| Error::Protocol("string is not UTF-8".into()))
    }
    /// `C`: the result's columns.
    fn columns(&mut self) -> Result<Vec<Column>, Error> {
        let n = self.u16()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.str()?;
            let tag = self.u8()?;
            let ty = match tag {
                10 => Type::Array(Box::new(type_of_tag(self.u8()?))),
                11 => match self.str()? {
                    n if n == "uuid" => Type::Uuid,
                    n => Type::Other(n),
                },
                20 => Type::Enum(self.str()?),
                _ => type_of_tag(tag),
            };
            columns.push(Column { name, ty });
        }
        Ok(columns)
    }
    /// `D`: a row of `n` values.
    fn row(&mut self, n: usize) -> Result<Vec<Value>, Error> {
        let mut row = Vec::with_capacity(n);
        for _ in 0..n {
            row.push(Value::decode(self)?);
        }
        Ok(row)
    }
    /// `K`: the command tag and count.
    fn tag(&mut self) -> Result<(String, u64), Error> {
        let command = self.str()?;
        let count = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        Ok((command, count))
    }
}

/// A SCRAM nonce from the process's randomly keyed hasher (the operating
/// system seeds it): printable, without commas.
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = Vec::with_capacity(24);
    for i in 0..3u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        bytes.extend_from_slice(&h.finish().to_le_bytes());
    }
    nucleus_crypto::base64_encode(&bytes)
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![kind];
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(body);
    m
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

/// A connection to a nucleus server.
pub struct Client {
    /// Reads are buffered; writes go out a frame at a time.
    conn: BufReader<Box<dyn Stream>>,
    status: TxnStatus,
    server: String,
    session: u32,
    /// The protocol version agreed with the server.
    version: u16,
    /// A message read while starting a `COPY` that did not begin.
    copy_pending: Option<(u8, Vec<u8>)>,
    config: Config,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.server)
            .field("session", &self.session)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connects and says Hello.
    pub fn connect(config: Config) -> Result<Self, Error> {
        use std::net::ToSocketAddrs;
        // Each address the name has, in turn (`localhost`: `::1`, then
        // `127.0.0.1`).
        let mut last = None;
        let mut connected = None;
        for addr in (config.host.as_str(), config.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, config.connect_timeout) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let stream = match (connected, last) {
            (Some(s), _) => s,
            (None, Some(e)) => return Err(e.into()),
            (None, None) => {
                return Err(Error::Protocol(format!("cannot resolve {}", config.host)));
            }
        };
        stream.set_nodelay(true)?;
        let (stream, binding): (Box<dyn Stream>, _) = match &config.tls {
            Some(tls) => {
                let s = tls.connect(stream, &config.host)?;
                (s.stream, s.channel_binding)
            }
            None => (Box::new(stream), None),
        };
        let mut c = Client {
            conn: BufReader::new(stream),
            status: TxnStatus::Idle,
            server: String::new(),
            session: 0,
            version: PROTOCOL_VERSION,
            copy_pending: None,
            config,
        };
        // Version 4: no password in Hello; the server asks for a proof.
        let mut hello = PROTOCOL_VERSION.to_le_bytes().to_vec();
        for s in [
            &c.config.user,
            &c.config.database,
            "",
            &c.config.client_name,
        ] {
            put_str(&mut hello, s);
        }
        c.conn.get_mut().write_all(&frame(b'H', &hello))?;
        c.conn.get_mut().flush()?;
        let mut scram: Option<nucleus_crypto::ScramClient> = None;
        loop {
            let (kind, body) = c.read()?;
            let mut b = Input { b: &body };
            match kind {
                b'A' => {
                    let msg = b.str()?;
                    let reply = match scram.as_mut() {
                        None => {
                            // The mechanisms offered: PLUS too over TLS.
                            let offered: Vec<&str> = msg.split(' ').collect();
                            if let Some(token) = &c.config.token {
                                // OAUTHBEARER's initial response (RFC 7628).
                                if !offered.contains(&"OAUTHBEARER") {
                                    return Err(Error::Protocol(
                                        "the server does not take bearer tokens".into(),
                                    ));
                                }
                                let reply = format!("n,,\x01auth=Bearer {token}\x01\x01");
                                let mut body = Vec::new();
                                put_str(&mut body, &reply);
                                c.conn.get_mut().write_all(&frame(b'a', &body))?;
                                c.conn.get_mut().flush()?;
                                continue;
                            }
                            if offered.contains(&"PLAIN") && !offered.contains(&"SCRAM-SHA-256") {
                                // A directory checks this role's password:
                                // the password itself, never without TLS.
                                if c.config.tls.is_none() {
                                    return Err(Error::Protocol(
                                        "the server asks for the password itself, which needs TLS"
                                            .into(),
                                    ));
                                }
                                let password = c.config.password.clone().unwrap_or_default();
                                let mut body = Vec::new();
                                put_str(&mut body, &format!("\0{}\0{password}", c.config.user));
                                c.conn.get_mut().write_all(&frame(b'a', &body))?;
                                c.conn.get_mut().flush()?;
                                continue;
                            }
                            if !offered.contains(&"SCRAM-SHA-256") {
                                return Err(Error::Protocol(format!(
                                    "unsupported authentication {msg:?}"
                                )));
                            }
                            let password = c.config.password.clone().unwrap_or_default();
                            let mut client = nucleus_crypto::ScramClient::new(&password, &nonce());
                            if let Some(data) = binding.clone() {
                                let plus = offered.contains(&"SCRAM-SHA-256-PLUS");
                                client = client.with_channel_binding(data, plus);
                            }
                            let first = client.first();
                            scram = Some(client);
                            Some(first)
                        }
                        Some(client) if msg.starts_with("v=") => {
                            client.verify(&msg).map_err(|_| {
                                Error::Protocol(
                                    "the server could not prove it knows the password".into(),
                                )
                            })?;
                            None
                        }
                        Some(client) => Some(
                            client
                                .respond(&msg)
                                .map_err(|_| Error::Protocol("malformed SCRAM message".into()))?,
                        ),
                    };
                    if let Some(r) = reply {
                        let mut body = Vec::new();
                        put_str(&mut body, &r);
                        c.conn.get_mut().write_all(&frame(b'a', &body))?;
                        c.conn.get_mut().flush()?;
                    }
                }
                b'W' => {
                    c.version = b.u16()?;
                    c.server = b.str()?;
                    c.session = b.u32()?;
                }
                b'R' => {
                    c.set_status(&body);
                    return Ok(c);
                }
                b'E' => return Err(Error::Server(b.server_error()?)),
                k => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        k as char
                    )));
                }
            }
        }
    }

    fn read(&mut self) -> Result<(u8, Vec<u8>), Error> {
        let mut h = [0u8; 5];
        self.conn.read_exact(&mut h)?;
        let len = u32::from_le_bytes(h[1..5].try_into().unwrap());
        if len > 1 << 30 {
            return Err(Error::Protocol("message too large".into()));
        }
        let mut body = vec![0u8; len as usize];
        self.conn.read_exact(&mut body)?;
        Ok((h[0], body))
    }

    fn set_status(&mut self, body: &[u8]) {
        self.status = match body.first() {
            Some(1) => TxnStatus::InBlock,
            Some(2) => TxnStatus::Failed,
            _ => TxnStatus::Idle,
        };
    }

    fn send(&mut self, kind: u8, body: &[u8]) -> Result<(), Error> {
        self.conn.get_mut().write_all(&frame(kind, body))?;
        self.conn.get_mut().flush()?;
        Ok(())
    }

    /// Sends `Q`: rows `fetch` at a time (0: all at once; from version 6).
    fn send_query(
        &mut self,
        sql: &str,
        params: &[Value],
        format: Format,
        fetch: u32,
    ) -> Result<(), Error> {
        let mut body = Vec::new();
        put_str(&mut body, sql);
        body.push(match format {
            Format::Typed => 0,
            Format::Text => 1,
        });
        if self.version >= 6 {
            body.extend_from_slice(&fetch.to_le_bytes());
        }
        body.extend_from_slice(&(params.len() as u16).to_le_bytes());
        for p in params {
            p.encode(&mut body);
        }
        self.send(b'Q', &body)
    }

    /// Runs `sql` (statements separated by `;`; with parameters, one
    /// statement).  Returns each statement's result; a failing statement
    /// ends the list with its error.  `Err` is for the connection.
    pub fn query(
        &mut self,
        sql: &str,
        params: &[Value],
        format: Format,
    ) -> Result<Vec<Result<Reply, ServerError>>, Error> {
        self.send_query(sql, params, format, 0)?;
        let mut out = Vec::new();
        let mut current: Option<(Vec<Column>, Vec<Vec<Value>>)> = None;
        loop {
            let (kind, body) = self.read()?;
            let mut b = Input { b: &body };
            match kind {
                b'C' => current = Some((b.columns()?, Vec::new())),
                b'D' => {
                    let (columns, rows) = current
                        .as_mut()
                        .ok_or_else(|| Error::Protocol("a row before its columns".into()))?;
                    rows.push(b.row(columns.len())?);
                }
                b'K' => {
                    let (command, count) = b.tag()?;
                    let (columns, rows, has_rows) = match current.take() {
                        Some((c, r)) => (c, r, true),
                        None => (Vec::new(), Vec::new(), false),
                    };
                    out.push(Ok(Reply {
                        columns,
                        rows,
                        command,
                        count,
                        has_rows,
                    }));
                }
                b'E' => {
                    current = None;
                    out.push(Err(b.server_error()?));
                }
                // A COPY wants data this call has none of.
                b'I' => self.refuse_copy()?,
                b'R' => {
                    self.set_status(&body);
                    return Ok(out);
                }
                k => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        k as char
                    )));
                }
            }
        }
    }

    /// Runs one statement and reads its rows as they arrive, `fetch` at a
    /// time (0: [`DEFAULT_FETCH`]): the server sends a batch and waits
    /// until the cursor has read it, so neither side holds more than a
    /// batch.  A cursor dropped before its end tells the server to stop
    /// (a server older than protocol version 6 sends everything at once,
    /// and the rest is read and skipped).  The server's error, if the
    /// statement fails before its first row, is [`Error::Server`]; after
    /// it, the cursor's last item.
    pub fn cursor(&mut self, sql: &str, params: &[Value], fetch: u32) -> Result<Cursor<'_>, Error> {
        let fetch = match fetch {
            _ if self.version < 6 => 0,
            0 => DEFAULT_FETCH,
            n => n,
        };
        self.open(sql, params, 0, fetch)
    }

    /// Runs one statement and reads its result a batch of `rows` at a time
    /// (0: the server's choice), each column a typed buffer — no value per
    /// cell (protocol version 7).  As [`Client::cursor`] otherwise: the
    /// server waits for each batch to be read, and dropping it early stops
    /// the statement.
    pub fn batches(
        &mut self,
        sql: &str,
        params: &[Value],
        rows: u32,
    ) -> Result<Batches<'_>, Error> {
        if self.version < 7 {
            return Err(Error::Protocol(
                "columnar results need protocol version 7; the server speaks an older one".into(),
            ));
        }
        Ok(Batches(self.open(sql, params, 2, rows)?))
    }

    /// Sends `Q` in result format `format` and reads up to the columns.
    fn open(
        &mut self,
        sql: &str,
        params: &[Value],
        format: u8,
        fetch: u32,
    ) -> Result<Cursor<'_>, Error> {
        let mut body = Vec::new();
        put_str(&mut body, sql);
        body.push(format);
        if self.version >= 6 {
            body.extend_from_slice(&fetch.to_le_bytes());
        }
        body.extend_from_slice(&(params.len() as u16).to_le_bytes());
        for p in params {
            p.encode(&mut body);
        }
        self.send(b'Q', &body)?;
        let mut done = None;
        let mut failed = None;
        loop {
            let (kind, body) = self.read()?;
            let mut b = Input { b: &body };
            match kind {
                b'C' => {
                    return Ok(Cursor {
                        columns: b.columns()?,
                        client: self,
                        fetch,
                        done: None,
                        failed: None,
                        ready: false,
                    });
                }
                b'K' => done = Some(b.tag()?),
                b'E' => failed = Some(b.server_error()?),
                b'I' => self.refuse_copy()?,
                b'R' => {
                    self.set_status(&body);
                    if let Some(e) = failed {
                        return Err(Error::Server(e));
                    }
                    return Ok(Cursor {
                        columns: Vec::new(),
                        client: self,
                        fetch,
                        done,
                        failed: None,
                        ready: true,
                    });
                }
                k => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        k as char
                    )));
                }
            }
        }
    }

    /// Runs `COPY … FROM STDIN` (one statement), sending what `data` reads —
    /// text or CSV, as the statement says — 64 KiB at a time; the rows it
    /// stored.  A failure to read `data` cancels the statement.
    pub fn copy_in(&mut self, sql: &str, data: &mut dyn Read) -> Result<u64, Error> {
        if self.copy_start(sql)?.is_none() {
            return self.copy_end(None);
        }
        let mut buf = vec![0u8; 1 << 16];
        let failed = loop {
            match data.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => self.send(b'd', &buf[..n])?,
                Err(e) => break Some(e.to_string()),
            }
        };
        self.copy_end(failed)
    }

    /// Runs `COPY t [(columns)] FROM STDIN`, sending `rows` as columnar
    /// batches of 65536 (each value fitted to its column's type: numbers
    /// widened or narrowed when they fit); the rows stored.  Bulk loading
    /// without a statement per row.
    pub fn copy_rows<I>(&mut self, sql: &str, rows: I) -> Result<u64, Error>
    where
        I: IntoIterator<Item = Vec<Value>>,
    {
        let Some(columns) = self.copy_start(sql)? else {
            return self.copy_end(None);
        };
        let types: Vec<Type> = columns.iter().map(|c| c.ty.clone()).collect();
        let mut batch = Vec::with_capacity(1 << 16);
        let mut failed = None;
        for row in rows {
            batch.push(row);
            if batch.len() == 1 << 16 {
                match encode_batch(&types, &columns, &batch) {
                    Ok(b) => self.send(b'B', &b)?,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
                batch.clear();
            }
        }
        if failed.is_none() && !batch.is_empty() {
            match encode_batch(&types, &columns, &batch) {
                Ok(b) => self.send(b'B', &b)?,
                Err(e) => failed = Some(e),
            }
        }
        match failed {
            None => self.copy_end(None),
            Some(e) => {
                // The statement is cancelled; the driver's error is the one
                // to report.
                let _ = self.copy_end(Some(e.to_string()));
                Err(e)
            }
        }
    }

    /// Answers a `COPY` that wants data with `f`.
    fn refuse_copy(&mut self) -> Result<(), Error> {
        let mut body = Vec::new();
        put_str(
            &mut body,
            "this call sends no data: use Client::copy_in or copy_rows for COPY FROM STDIN",
        );
        self.send(b'f', &body)
    }

    /// Sends a `COPY` and reads up to `I`: the columns its data holds, or
    /// none when the statement ended without taking data (an error).
    fn copy_start(&mut self, sql: &str) -> Result<Option<Vec<Column>>, Error> {
        if self.version < 7 {
            return Err(Error::Protocol(
                "COPY FROM STDIN needs protocol version 7; the server speaks an older one".into(),
            ));
        }
        self.send_query(sql, &[], Format::Typed, 0)?;
        let (kind, body) = self.read()?;
        let mut b = Input { b: &body };
        match kind {
            b'I' => Ok(Some(b.columns()?)),
            // An error (or a statement that is not a COPY): the rest is read
            // by `copy_end`.
            _ => {
                self.copy_pending = Some((kind, body));
                Ok(None)
            }
        }
    }

    /// Ends the data (`e`, or `f` with why) and reads the statement's end.
    fn copy_end(&mut self, failed: Option<String>) -> Result<u64, Error> {
        let mut pending = self.copy_pending.take();
        if pending.is_none() {
            match failed {
                None => self.send(b'e', &[])?,
                Some(why) => {
                    let mut body = Vec::new();
                    put_str(&mut body, &why);
                    self.send(b'f', &body)?;
                }
            }
        }
        let (mut count, mut error) = (0, None);
        loop {
            let (kind, body) = match pending.take() {
                Some(m) => m,
                None => self.read()?,
            };
            let mut b = Input { b: &body };
            match kind {
                b'K' => count = b.tag()?.1,
                b'E' => error = Some(b.server_error()?),
                b'C' | b'D' => {}
                b'R' => {
                    self.set_status(&body);
                    return match error {
                        Some(e) => Err(Error::Server(e)),
                        None => Ok(count),
                    };
                }
                k => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        k as char
                    )));
                }
            }
        }
    }

    /// Runs one statement; its result, or the server's error as
    /// [`Error::Server`].
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Reply, Error> {
        let mut replies = self.query(sql, params, Format::Typed)?;
        match replies.pop() {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(Error::Server(e)),
            None => Err(Error::Protocol("no result".into())),
        }
    }

    /// The rows of one statement.
    pub fn rows(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, Error> {
        Ok(self.execute(sql, params)?.rows)
    }

    /// The transaction status after the last query.
    pub fn status(&self) -> TxnStatus {
        self.status
    }

    /// The server's name and version (`nucleus 0.1.0`).
    pub fn server_version(&self) -> &str {
        &self.server
    }

    /// The server's number for this session.
    pub fn session_id(&self) -> u32 {
        self.session
    }

    /// The configuration it connected with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Says Bye and closes.
    pub fn close(mut self) -> io::Result<()> {
        self.conn.get_mut().write_all(&frame(b'X', &[]))?;
        self.conn.get_mut().flush()
    }
}

/// One statement's rows as they arrive ([`Client::cursor`]); each item is
/// a row, or the error that ended the statement.
pub struct Cursor<'c> {
    client: &'c mut Client,
    columns: Vec<Column>,
    fetch: u32,
    done: Option<(String, u64)>,
    failed: Option<ServerError>,
    /// The server has said Ready: nothing more comes.
    ready: bool,
}

impl fmt::Debug for Cursor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("columns", &self.columns)
            .field("fetch", &self.fetch)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Cursor<'_> {
    /// The result's columns (empty for a statement without rows).
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The command tag and count (`SELECT`, 3), once every row is read.
    pub fn command(&self) -> Option<(&str, u64)> {
        self.done.as_ref().map(|(c, n)| (c.as_str(), *n))
    }

    /// Stops reading: the server is told to send no more, and what it
    /// already sent is skipped.  The command tag and count of the rows
    /// sent, or the error that ended the statement.
    pub fn close(mut self) -> Result<(String, u64), Error> {
        self.drain()?;
        if let Some(e) = self.failed.take() {
            return Err(Error::Server(e));
        }
        Ok(self.done.take().unwrap_or_default())
    }

    /// Reads to Ready, skipping rows and answering a pause with `c`.
    fn drain(&mut self) -> Result<(), Error> {
        while !self.ready {
            let (kind, body) = self.client.read().inspect_err(|_| self.ready = true)?;
            let mut b = Input { b: &body };
            match kind {
                b'D' | b'C' | b'B' => {}
                b'I' => self.client.refuse_copy()?,
                b'S' => self.client.send(b'c', &[])?,
                b'K' => self.done = Some(b.tag()?),
                b'E' => self.failed = Some(b.server_error()?),
                b'R' => {
                    self.client.set_status(&body);
                    self.ready = true;
                }
                k => {
                    self.ready = true;
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        k as char
                    )));
                }
            }
        }
        Ok(())
    }
}

impl Iterator for Cursor<'_> {
    type Item = Result<Vec<Value>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let n = self.columns.len();
        self.next_body(b'D')
            .map(|body| body.and_then(|b| Input { b: &b }.row(n)))
    }
}

impl Cursor<'_> {
    /// The body of the next `kind` message (`D` a row, `B` a batch).
    fn next_body(&mut self, data: u8) -> Option<Result<Vec<u8>, Error>> {
        while !self.ready {
            let (kind, body) = match self.client.read() {
                Ok(m) => m,
                Err(e) => {
                    self.ready = true;
                    return Some(Err(e));
                }
            };
            let mut b = Input { b: &body };
            let step = match kind {
                k if k == data => return Some(Ok(body)),
                // The batch is read: the next.
                b'S' => self.client.send(b'F', &self.fetch.to_le_bytes()),
                b'K' => b.tag().map(|t| self.done = Some(t)),
                b'E' => b.server_error().map(|e| self.failed = Some(e)),
                b'I' => self.client.refuse_copy(),
                b'R' => {
                    self.client.set_status(&body);
                    self.ready = true;
                    return self.failed.take().map(|e| Err(Error::Server(e)));
                }
                b'C' => Err(Error::Protocol(
                    "a cursor runs one statement; this query holds several".into(),
                )),
                k => Err(Error::Protocol(format!(
                    "unexpected message {:?}",
                    k as char
                ))),
            };
            if let Err(e) = step {
                self.ready = true;
                return Some(Err(e));
            }
        }
        None
    }
}

/// A value as column type `t` wants it (numbers widened or narrowed when
/// they fit, a timestamp of either kind), or none.
fn fit(v: &Value, t: &Type) -> Option<Value> {
    let int = match v {
        Value::SmallInt(i) => Some(i64::from(*i)),
        Value::Integer(i) => Some(i64::from(*i)),
        Value::Int(i) => Some(*i),
        _ => None,
    };
    Some(match (t, v) {
        (Type::Bool, Value::Bool(_))
        | (Type::Date, Value::Date(_))
        | (Type::Time, Value::Time(_))
        | (Type::Interval, Value::Interval { .. })
        | (Type::Uuid, Value::Uuid(_))
        | (Type::Numeric, Value::Numeric { .. }) => v.clone(),
        (Type::SmallInt, _) => Value::SmallInt(i16::try_from(int?).ok()?),
        (Type::Integer, _) => Value::Integer(i32::try_from(int?).ok()?),
        (Type::Int, _) => Value::Int(int?),
        (Type::Real, Value::Real(f)) => Value::Real(*f),
        (Type::Real, Value::Float(f)) => Value::Real(*f as f32),
        (Type::Real, _) => Value::Real(int? as f32),
        (Type::Float, Value::Float(f)) => Value::Float(*f),
        (Type::Float, Value::Real(f)) => Value::Float(f64::from(*f)),
        (Type::Float, _) => Value::Float(int? as f64),
        (Type::Numeric, _) => Value::Numeric {
            mantissa: i128::from(int?),
            scale: 0,
        },
        (Type::Timestamp, Value::Timestamp(t) | Value::TimestampTz(t)) => Value::Timestamp(*t),
        (Type::TimestampTz, Value::Timestamp(t) | Value::TimestampTz(t)) => Value::TimestampTz(*t),
        _ => return None,
    })
}

/// A batch of `rows` of columns `types`, as a client sends it for `COPY`
/// (DOCS §10: the columnar layout).
fn encode_batch(types: &[Type], columns: &[Column], rows: &[Vec<Value>]) -> Result<Vec<u8>, Error> {
    let pad = |b: &mut Vec<u8>| {
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
    };
    let mismatch = |c: usize, v: &Value| {
        Error::Value(format!(
            "{v:?} cannot be sent as column \"{}\" ({:?})",
            columns[c].name, types[c]
        ))
    };
    if let Some(r) = rows.iter().find(|r| r.len() != types.len()) {
        return Err(Error::Value(format!(
            "a row of {} values for {} columns",
            r.len(),
            types.len()
        )));
    }
    let mut b = (rows.len() as u32).to_le_bytes().to_vec();
    for (c, t) in types.iter().enumerate() {
        let nulls = rows.iter().any(|r| r[c].is_null());
        b.push(u8::from(nulls));
        if nulls {
            pad(&mut b);
            let mut bits = vec![0u8; rows.len().div_ceil(8)];
            for (i, r) in rows.iter().enumerate() {
                if !r[c].is_null() {
                    bits[i / 8] |= 1 << (i % 8);
                }
            }
            b.extend_from_slice(&bits);
        }
        pad(&mut b);
        let w = layout(t);
        if w > 0 {
            for r in rows {
                if r[c].is_null() {
                    b.resize(b.len() + w, 0);
                    continue;
                }
                let v = fit(&r[c], t).ok_or_else(|| mismatch(c, &r[c]))?;
                let mut tagged = Vec::new();
                v.encode(&mut tagged);
                // The payload behind the tag (a uuid's 16 bytes at the end).
                b.extend_from_slice(&tagged[tagged.len() - w..]);
            }
        } else {
            let mut data = Vec::new();
            let mut offsets = vec![0u8; 4];
            for r in rows {
                match (&r[c], t) {
                    (Value::Null, _) => {}
                    (Value::Bytes(x), Type::Bytea) => data.extend_from_slice(x),
                    (Value::Other { bytes, .. }, Type::Other(_)) => data.extend_from_slice(bytes),
                    (v @ (Value::Inet { .. } | Value::Cidr { .. }), Type::Inet | Type::Cidr) => {
                        let mut tagged = Vec::new();
                        v.encode(&mut tagged);
                        data.extend_from_slice(&tagged[1..]);
                    }
                    (v @ Value::Array(_), Type::Array(_)) => v.encode(&mut data),
                    (Value::Text(s), _) => data.extend_from_slice(s.as_bytes()),
                    (v, Type::Text | Type::Char | Type::Enum(_) | Type::Unknown) => {
                        data.extend_from_slice(v.to_string().as_bytes())
                    }
                    (v, _) => return Err(mismatch(c, v)),
                }
                offsets.extend_from_slice(&(data.len() as u32).to_le_bytes());
            }
            b.extend_from_slice(&offsets);
            pad(&mut b);
            b.extend_from_slice(&data);
        }
        pad(&mut b);
    }
    Ok(b)
}

/// One statement's result as it arrives, a batch of rows at a time with a
/// typed buffer per column ([`Client::batches`]).
#[derive(Debug)]
pub struct Batches<'c>(Cursor<'c>);

impl Batches<'_> {
    /// The result's columns.
    pub fn columns(&self) -> &[Column] {
        self.0.columns()
    }

    /// The command tag and count, once every batch is read.
    pub fn command(&self) -> Option<(&str, u64)> {
        self.0.command()
    }

    /// Stops reading, as [`Cursor::close`].
    pub fn close(self) -> Result<(String, u64), Error> {
        self.0.close()
    }
}

impl Iterator for Batches<'_> {
    type Item = Result<Batch, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let types: Vec<Type> = self.0.columns.iter().map(|c| c.ty.clone()).collect();
        self.0
            .next_body(b'B')
            .map(|body| body.and_then(|b| Batch::decode(&b, types)))
    }
}

/// Rows of a result, a buffer per column ([`Client::batches`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    rows: usize,
    types: Vec<Type>,
    columns: Vec<ColumnBatch>,
}

/// One column of a [`Batch`].
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnBatch {
    /// A bit per row, least significant first, set for a value (`None`:
    /// no NULLs).
    pub valid: Option<Vec<u8>>,
    /// The values, NULLs' slots zero or empty.
    pub data: ColumnData,
}

/// A column's values, by its type's layout.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnData {
    /// `boolean`.
    Bool(Vec<bool>),
    /// `smallint`.
    I16(Vec<i16>),
    /// `integer`, and `date` (days since 2000-01-01).
    I32(Vec<i32>),
    /// `bigint`, and `timestamp`, `timestamptz` (µs since 2000-01-01) and
    /// `time` (µs since midnight).
    I64(Vec<i64>),
    /// `real`.
    F32(Vec<f32>),
    /// `double precision`.
    F64(Vec<f64>),
    /// 16 bytes each: `uuid`, or `interval` (`i32` months, `i32` days, `i64`
    /// µs).
    Fixed16(Vec<[u8; 16]>),
    /// `numeric`: mantissa and scale.
    Numeric(Vec<(i128, u8)>),
    /// Variable width: value `i` is `data[offsets[i]..offsets[i + 1]]` —
    /// UTF-8 for text, `char(n)`, enums and unknown types; the bytes of a
    /// `bytea` or a registered type; family, mask length and address for
    /// `inet` and `cidr`; an array in the row format's value encoding.
    Var {
        /// One more than the rows.
        offsets: Vec<u32>,
        /// The values' bytes.
        data: Vec<u8>,
    },
}

/// How a column of a type is laid out in a batch (DOCS §10).
fn layout(t: &Type) -> usize {
    match t {
        Type::Bool => 1,
        Type::SmallInt => 2,
        Type::Integer | Type::Real | Type::Date => 4,
        Type::Int | Type::Float | Type::Timestamp | Type::TimestampTz | Type::Time => 8,
        Type::Interval | Type::Uuid => 16,
        Type::Numeric => 17,
        _ => 0,
    }
}

impl Batch {
    fn decode(body: &[u8], types: Vec<Type>) -> Result<Batch, Error> {
        let bad = || Error::Protocol("malformed batch".into());
        let mut at = 0usize;
        let take = |at: &mut usize, n: usize| -> Result<&[u8], Error> {
            let end = at
                .checked_add(n)
                .filter(|&e| e <= body.len())
                .ok_or_else(bad)?;
            let s = &body[*at..end];
            *at = end;
            Ok(s)
        };
        let align = |at: &mut usize| *at = at.div_ceil(8) * 8;
        let rows = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap()) as usize;
        let mut columns = Vec::with_capacity(types.len());
        for t in &types {
            let nulls = take(&mut at, 1)?[0] != 0;
            let valid = if nulls {
                align(&mut at);
                Some(take(&mut at, rows.div_ceil(8))?.to_vec())
            } else {
                None
            };
            align(&mut at);
            let w = layout(t);
            let data = if w == 0 {
                let raw = take(&mut at, (rows + 1) * 4)?;
                let offsets: Vec<u32> = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_le_bytes(*c))
                    .collect();
                align(&mut at);
                let n = *offsets.last().unwrap_or(&0) as usize;
                if offsets.windows(2).any(|w| w[0] > w[1]) {
                    return Err(bad());
                }
                ColumnData::Var {
                    data: take(&mut at, n)?.to_vec(),
                    offsets,
                }
            } else {
                let raw = take(&mut at, rows * w)?;
                let chunks = raw.chunks_exact(w);
                match t {
                    Type::Bool => ColumnData::Bool(chunks.map(|c| c[0] != 0).collect()),
                    Type::SmallInt => ColumnData::I16(
                        chunks
                            .map(|c| i16::from_le_bytes(c.try_into().unwrap()))
                            .collect(),
                    ),
                    Type::Integer | Type::Date => ColumnData::I32(
                        chunks
                            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
                            .collect(),
                    ),
                    Type::Real => ColumnData::F32(
                        chunks
                            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                            .collect(),
                    ),
                    Type::Float => ColumnData::F64(
                        chunks
                            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
                            .collect(),
                    ),
                    Type::Numeric => ColumnData::Numeric(
                        chunks
                            .map(|c| (i128::from_le_bytes(c[..16].try_into().unwrap()), c[16]))
                            .collect(),
                    ),
                    Type::Interval | Type::Uuid => {
                        ColumnData::Fixed16(chunks.map(|c| c.try_into().unwrap()).collect())
                    }
                    _ => ColumnData::I64(
                        chunks
                            .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
                            .collect(),
                    ),
                }
            };
            align(&mut at);
            columns.push(ColumnBatch { valid, data });
        }
        Ok(Batch {
            rows,
            types,
            columns,
        })
    }

    /// Its rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Column `i`.
    pub fn column(&self, i: usize) -> &ColumnBatch {
        &self.columns[i]
    }

    /// The value at `row` of column `col`, as [`Client::cursor`] would give
    /// it (building one: for reading a cell, not a column).
    pub fn value(&self, row: usize, col: usize) -> Result<Value, Error> {
        let c = &self.columns[col];
        if c.is_null(row) {
            return Ok(Value::Null);
        }
        let t = &self.types[col];
        Ok(match &c.data {
            ColumnData::Bool(v) => Value::Bool(v[row]),
            ColumnData::I16(v) => Value::SmallInt(v[row]),
            ColumnData::I32(v) if *t == Type::Date => Value::Date(v[row]),
            ColumnData::I32(v) => Value::Integer(v[row]),
            ColumnData::I64(v) => match t {
                Type::Timestamp => Value::Timestamp(v[row]),
                Type::TimestampTz => Value::TimestampTz(v[row]),
                Type::Time => Value::Time(v[row]),
                _ => Value::Int(v[row]),
            },
            ColumnData::F32(v) => Value::Real(v[row]),
            ColumnData::F64(v) => Value::Float(v[row]),
            ColumnData::Numeric(v) => Value::Numeric {
                mantissa: v[row].0,
                scale: v[row].1,
            },
            ColumnData::Fixed16(v) if *t == Type::Uuid => Value::Uuid(v[row]),
            ColumnData::Fixed16(v) => {
                let b = &v[row];
                Value::Interval {
                    months: i32::from_le_bytes(b[0..4].try_into().unwrap()),
                    days: i32::from_le_bytes(b[4..8].try_into().unwrap()),
                    micros: i64::from_le_bytes(b[8..16].try_into().unwrap()),
                }
            }
            ColumnData::Var { .. } => {
                let b = c.bytes(row).unwrap_or_default();
                match t {
                    Type::Bytea => Value::Bytes(b.to_vec()),
                    Type::Other(name) => Value::Other {
                        type_name: name.clone(),
                        bytes: b.to_vec(),
                    },
                    Type::Inet | Type::Cidr => {
                        // The row format's payload, behind its tag.
                        let mut tagged = vec![if *t == Type::Inet { 18 } else { 19 }];
                        tagged.extend_from_slice(b);
                        Value::decode(&mut Input { b: &tagged })?
                    }
                    Type::Array(_) => Value::decode(&mut Input { b })?,
                    _ => Value::Text(
                        String::from_utf8(b.to_vec())
                            .map_err(|_| Error::Protocol("text is not UTF-8".into()))?,
                    ),
                }
            }
        })
    }
}

impl ColumnBatch {
    /// Whether the value at `row` is NULL.
    pub fn is_null(&self, row: usize) -> bool {
        self.valid
            .as_ref()
            .is_some_and(|v| v[row / 8] & (1 << (row % 8)) == 0)
    }

    /// The `integer` (or `date`) values.
    pub fn i32s(&self) -> Option<&[i32]> {
        match &self.data {
            ColumnData::I32(v) => Some(v),
            _ => None,
        }
    }

    /// The `bigint` (or timestamp, time) values.
    pub fn i64s(&self) -> Option<&[i64]> {
        match &self.data {
            ColumnData::I64(v) => Some(v),
            _ => None,
        }
    }

    /// The `double precision` values.
    pub fn f64s(&self) -> Option<&[f64]> {
        match &self.data {
            ColumnData::F64(v) => Some(v),
            _ => None,
        }
    }

    /// A variable-width value's bytes (`None` for NULL or a fixed width).
    pub fn bytes(&self, row: usize) -> Option<&[u8]> {
        match &self.data {
            ColumnData::Var { offsets, data } if !self.is_null(row) => {
                Some(&data[offsets[row] as usize..offsets[row + 1] as usize])
            }
            _ => None,
        }
    }

    /// A text value (`None` for NULL, a fixed width or bytes that are not
    /// UTF-8).
    pub fn str(&self, row: usize) -> Option<&str> {
        self.bytes(row).and_then(|b| std::str::from_utf8(b).ok())
    }
}

impl Drop for Cursor<'_> {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}
