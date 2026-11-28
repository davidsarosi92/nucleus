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

pub mod columns;

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream};
use std::time::Duration;

use nucleus_wire::{
    Malformed, Reader, WireValue, columnar, frame, put, read_frame, read_value, tag,
};

/// The protocol version this driver speaks (2: registered types such as
/// `uuid` arrive in their binary form; 3: `smallint`, `integer`, `real`,
/// `bytea`, `time`, `inet` and `cidr` in theirs, `char(n)` and enums
/// named as such; 4: the password is proved by SCRAM-SHA-256, never
/// sent; 5: errors carry a hint; 6: results stream, and a [`Cursor`]
/// fetches them in batches; 7: columnar results, [`Client::batches`]; 8:
/// subject keys; 9: requests on a collection's documents,
/// [`Client::collection`]; 10: waiting on a stream, [`Client::stream_of`]).
pub const PROTOCOL_VERSION: u16 = 10;
/// A [`Cursor`]'s batch when none is given: rows the server sends before
/// it waits for the client.
pub const DEFAULT_FETCH: u32 = 1000;
/// The nucleus protocol's default port.
pub const DEFAULT_PORT: u16 = 20_035;

/// Where and as whom to connect.
#[derive(Clone, Debug)]
pub struct Config {
    /// Host name or address — or, beginning with `/`, the path of the
    /// server's Unix socket (`nucleus server --socket`; the port unused).
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
    /// `vector`: `f32` components.
    Vector,
    /// `sparsevec`: dimensions and non-zero components.
    SparseVector,
    /// `json`: its values as their text.
    Json,
    /// Another registered type, by name.
    Other(String),
}

/// A vector's binary form: its components as little-endian `f32`s.
fn vector_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// A vector from its binary form.
fn vector_of(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// A sparse vector's binary form: its dimensions and its non-zero
/// components' count, their positions and their values, little-endian
/// `u32`s and `f32`s.
fn sparse_bytes(dims: u32, indices: &[u32], values: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + 8 * indices.len());
    b.extend_from_slice(&dims.to_le_bytes());
    b.extend_from_slice(&(indices.len() as u32).to_le_bytes());
    indices
        .iter()
        .for_each(|i| b.extend_from_slice(&i.to_le_bytes()));
    values
        .iter()
        .for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
    b
}

/// A sparse vector from its binary form (`None`: not one).
fn sparse_of(b: &[u8]) -> Option<Value> {
    let (words, rest) = b.as_chunks::<4>();
    if !rest.is_empty() || words.len() < 2 {
        return None;
    }
    let n = u32::from_le_bytes(words[1]) as usize;
    if words.len() != 2 + 2 * n {
        return None;
    }
    Some(Value::SparseVector {
        dims: u32::from_le_bytes(words[0]),
        indices: words[2..2 + n]
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect(),
        values: words[2 + n..]
            .iter()
            .map(|w| f32::from_le_bytes(*w))
            .collect(),
    })
}

fn type_of_tag(t: u8) -> Type {
    match t {
        tag::BOOL => Type::Bool,
        tag::BIGINT => Type::Int,
        tag::DOUBLE => Type::Float,
        tag::TEXT => Type::Text,
        tag::NUMERIC => Type::Numeric,
        tag::DATE => Type::Date,
        tag::TIMESTAMP => Type::Timestamp,
        tag::TIMESTAMPTZ => Type::TimestampTz,
        tag::INTERVAL => Type::Interval,
        tag::SMALLINT => Type::SmallInt,
        tag::INTEGER => Type::Integer,
        tag::REAL => Type::Real,
        tag::BYTEA => Type::Bytea,
        tag::TIME => Type::Time,
        tag::CHAR => Type::Char,
        tag::INET => Type::Inet,
        tag::CIDR => Type::Cidr,
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
    /// A `vector`: its components.
    Vector(Vec<f32>),
    /// A `sparsevec`: its dimensions and its non-zero components —
    /// positions from 0, ascending, each with its value (the text form
    /// counts from 1: `{1:0.5,4:2}/10`).
    SparseVector {
        /// The dimensions.
        dims: u32,
        /// The non-zero components' positions.
        indices: Vec<u32>,
        /// Their values.
        values: Vec<f32>,
    },
    /// A `json` value: its text (the server's: `{"a": 1, "b": [1, 2]}`;
    /// as a parameter, any JSON text).
    Json(String),
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

impl From<Vec<f32>> for Value {
    fn from(v: Vec<f32>) -> Self {
        Value::Vector(v)
    }
}

impl From<&[f32]> for Value {
    fn from(v: &[f32]) -> Self {
        Value::Vector(v.to_vec())
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
            Value::Null => put::null(out),
            Value::Bool(b) => put::bool(out, *b),
            Value::SmallInt(i) => put::smallint(out, *i),
            Value::Integer(i) => put::integer(out, *i),
            Value::Int(i) => put::bigint(out, *i),
            Value::Real(f) => put::real(out, *f),
            Value::Float(f) => put::double(out, *f),
            Value::Bytes(b) => put::bytea(out, b),
            Value::Time(t) => put::time(out, *t),
            Value::Inet { addr, bits } | Value::Cidr { addr, bits } => {
                let cidr = matches!(self, Value::Cidr { .. });
                match addr {
                    IpAddr::V4(a) => put::net(out, cidr, 4, *bits, &a.octets()),
                    IpAddr::V6(a) => put::net(out, cidr, 6, *bits, &a.octets()),
                }
            }
            Value::Text(s) => put::text(out, s),
            Value::Numeric { mantissa, scale } => put::numeric(out, *mantissa, *scale),
            Value::Date(d) => put::date(out, *d),
            Value::Timestamp(t) => put::timestamp(out, *t, false),
            Value::TimestampTz(t) => put::timestamp(out, *t, true),
            Value::Interval {
                months,
                days,
                micros,
            } => put::interval(out, *months, *days, *micros),
            Value::Array(items) => {
                put::array(out, items.len());
                for v in items {
                    v.encode(out);
                }
            }
            Value::Uuid(b) => put::ext(out, "uuid", b),
            Value::Vector(v) => put::ext(out, "vector", &vector_bytes(v)),
            Value::SparseVector {
                dims,
                indices,
                values,
            } => put::ext(out, "sparsevec", &sparse_bytes(*dims, indices, values)),
            Value::Json(text) => put::ext(out, "json", text.as_bytes()),
            Value::Other { type_name, bytes } => put::ext(out, type_name, bytes),
        }
    }

    fn decode(r: &mut Input) -> Result<Value, Error> {
        Ok(Value::from_wire(read_value(&mut r.r)?))
    }

    /// The driver's value for what the protocol carried.
    fn from_wire(v: WireValue) -> Value {
        match v {
            WireValue::Null => Value::Null,
            WireValue::Bool(b) => Value::Bool(b),
            WireValue::SmallInt(i) => Value::SmallInt(i),
            WireValue::Integer(i) => Value::Integer(i),
            WireValue::BigInt(i) => Value::Int(i),
            WireValue::Real(f) => Value::Real(f),
            WireValue::Double(f) => Value::Float(f),
            WireValue::Text(s) => Value::Text(s),
            WireValue::Numeric { mantissa, scale } => Value::Numeric { mantissa, scale },
            WireValue::Date(d) => Value::Date(d),
            WireValue::Timestamp(t) => Value::Timestamp(t),
            WireValue::TimestampTz(t) => Value::TimestampTz(t),
            WireValue::Interval {
                months,
                days,
                micros,
            } => Value::Interval {
                months,
                days,
                micros,
            },
            WireValue::Array(items) => {
                Value::Array(items.into_iter().map(Value::from_wire).collect())
            }
            WireValue::Ext { type_name, bytes } => {
                match (type_name.as_str(), <[u8; 16]>::try_from(bytes.as_slice())) {
                    ("uuid", Ok(b)) => Value::Uuid(b),
                    ("vector", _) => Value::Vector(vector_of(&bytes)),
                    ("json", _) => match String::from_utf8(bytes) {
                        Ok(text) => Value::Json(text),
                        Err(e) => Value::Other {
                            type_name,
                            bytes: e.into_bytes(),
                        },
                    },
                    ("sparsevec", _) if sparse_of(&bytes).is_some() => {
                        sparse_of(&bytes).expect("checked")
                    }
                    _ => Value::Other { type_name, bytes },
                }
            }
            WireValue::Bytea(b) => Value::Bytes(b),
            WireValue::Time(t) => Value::Time(t),
            WireValue::Net {
                cidr,
                family,
                bits,
                addr,
            } => {
                let addr = if family == 4 {
                    IpAddr::V4(Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]))
                } else {
                    IpAddr::V6(Ipv6Addr::from(addr))
                };
                if cidr {
                    Value::Cidr { addr, bits }
                } else {
                    Value::Inet { addr, bits }
                }
            }
        }
    }

    /// Whether this is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
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
            Value::Text(s) | Value::Json(s) => f.write_str(s),
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
            Value::Vector(v) => {
                f.write_str("[")?;
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{c}")?;
                }
                f.write_str("]")
            }
            Value::SparseVector {
                dims,
                indices,
                values,
            } => {
                f.write_str("{")?;
                for (i, (at, v)) in indices.iter().zip(values).enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{}:{v}", u64::from(*at) + 1)?;
                }
                write!(f, "}}/{dims}")
            }
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
    r: Reader<'a>,
}

impl From<Malformed> for Error {
    fn from(e: Malformed) -> Self {
        Error::Protocol(e.0.into())
    }
}

impl<'a> Input<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { r: Reader::new(b) }
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.r.u8()?)
    }
    /// The `E` message's fields: the hint, from version 5, follows.
    fn server_error(&mut self) -> Result<ServerError, Error> {
        let code = self.str()?;
        let message = self.str()?;
        let hint = if self.r.is_empty() {
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
        Ok(self.r.u16()?)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(self.r.u32()?)
    }
    fn str(&mut self) -> Result<String, Error> {
        Ok(self.r.str()?)
    }
    /// `C`: the result's columns.
    fn columns(&mut self) -> Result<Vec<Column>, Error> {
        let n = self.u16()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.str()?;
            let tag = self.u8()?;
            let ty = match tag {
                tag::ARRAY => Type::Array(Box::new(type_of_tag(self.u8()?))),
                tag::EXT => match self.str()? {
                    n if n == "uuid" => Type::Uuid,
                    n if n == "vector" => Type::Vector,
                    n if n == "sparsevec" => Type::SparseVector,
                    n if n == "json" => Type::Json,
                    n => Type::Other(n),
                },
                tag::ENUM => Type::Enum(self.str()?),
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
        let count = self.r.u64()?;
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

fn put_str(b: &mut Vec<u8>, s: &str) {
    put::str(b, s);
}

/// How many bytes of statements [`Client::pipeline`] sends before it
/// reads their replies: less than a socket's buffers hold (16 KiB sent
/// plus 128 KiB received, Linux's and macOS's defaults), so a window is
/// written whole while the server is held up writing replies nobody
/// reads yet.
pub const PIPELINE_WINDOW: usize = 32 << 10;

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
        // A path: the server's Unix socket, on this machine — no TLS to
        // ask for there.
        if config.host.starts_with('/') {
            if config.tls.is_some() {
                return Err(Error::Protocol(
                    "TLS is not offered on a Unix socket".into(),
                ));
            }
            #[cfg(unix)]
            {
                let stream = std::os::unix::net::UnixStream::connect(&config.host)?;
                return Self::hello(Box::new(stream), None, config);
            }
            #[cfg(not(unix))]
            return Err(Error::Protocol(format!(
                "{}: Unix sockets are not supported here",
                config.host
            )));
        }
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
        Self::hello(stream, binding, config)
    }

    /// Says Hello over `stream`, a connection already made — to a
    /// database in this process (`nucleus-embedded`'s, PLAN.md §M14.23),
    /// say; `config`'s address and TLS are not used.
    pub fn over(stream: Box<dyn Stream>, config: Config) -> Result<Self, Error> {
        Self::hello(stream, None, config)
    }

    fn hello(
        stream: Box<dyn Stream>,
        binding: Option<Vec<u8>>,
        config: Config,
    ) -> Result<Self, Error> {
        let mut c = Client {
            conn: BufReader::with_capacity(64 << 10, stream),
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
            let mut b = Input::new(&body);
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
        read_frame(&mut self.conn).map_err(|e| match e.kind() {
            io::ErrorKind::InvalidData => Error::Protocol(e.to_string()),
            _ => Error::Io(e),
        })
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
        let body = self.query_body(sql, params, format, fetch);
        self.send(b'Q', &body)
    }

    /// The body of a `Q`.
    fn query_body(&self, sql: &str, params: &[Value], format: Format, fetch: u32) -> Vec<u8> {
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
        body
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
        self.replies()
    }

    /// Runs each of `statements` as [`Client::execute`] would — its
    /// result, or the server's error in its place — sending them without
    /// waiting for the replies before (pipelined): a round trip for many.
    /// Each runs on its own, as if sent alone: one that fails does not
    /// stop the rest (inside `BEGIN` … `COMMIT` the rest fail as the
    /// transaction has), and a text of several statements gives its last
    /// one's result (none: an empty reply).  No `COPY FROM STDIN` (the
    /// server would read the next statement as its data, and close the
    /// connection).  They go out [`PIPELINE_WINDOW`] bytes at a time, each
    /// window's replies read before the next is sent: a window fits in
    /// the sockets' buffers, so neither side waits on the other's writes.
    /// `Err` is for the connection.
    pub fn pipeline(
        &mut self,
        statements: &[(&str, &[Value])],
    ) -> Result<Vec<Result<Reply, ServerError>>, Error> {
        let mut out = Vec::with_capacity(statements.len());
        let mut rest = statements;
        while !rest.is_empty() {
            let mut window = Vec::new();
            let mut n = 0;
            for (sql, params) in rest {
                let m = frame(b'Q', &self.query_body(sql, params, Format::Typed, 0));
                if n > 0 && window.len() + m.len() > PIPELINE_WINDOW {
                    break;
                }
                window.extend_from_slice(&m);
                n += 1;
            }
            self.conn.get_mut().write_all(&window)?;
            self.conn.get_mut().flush()?;
            for _ in 0..n {
                out.push(self.replies()?.pop().unwrap_or_else(|| {
                    Ok(Reply {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        command: String::new(),
                        count: 0,
                        has_rows: false,
                    })
                }));
            }
            rest = &rest[n..];
        }
        Ok(out)
    }

    /// Runs `sql` once for each of `rows` (its parameters), pipelined
    /// ([`Client::pipeline`]), all or none: outside a transaction block
    /// in one of their own (one commit for all), inside one in it.  The
    /// rows affected or returned, all told; the first failure is
    /// [`Error::Server`] — and nothing it ran outside a block stays (the
    /// block it ran in, if one was open, has failed).
    pub fn execute_many(&mut self, sql: &str, rows: &[&[Value]]) -> Result<u64, Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let own = self.status == TxnStatus::Idle;
        let mut statements: Vec<(&str, &[Value])> = Vec::with_capacity(rows.len() + 2);
        if own {
            statements.push(("BEGIN", &[]));
        }
        statements.extend(rows.iter().map(|r| (sql, *r)));
        if own {
            statements.push(("COMMIT", &[]));
        }
        let mut count = 0;
        for r in self.pipeline(&statements)? {
            count += r.map_err(Error::Server)?.count;
        }
        Ok(count)
    }

    /// The replies to one `Q`, up to its `R`.
    fn replies(&mut self) -> Result<Vec<Result<Reply, ServerError>>, Error> {
        let mut out = Vec::new();
        let mut current: Option<(Vec<Column>, Vec<Vec<Value>>)> = None;
        loop {
            let (kind, body) = self.read()?;
            let mut b = Input::new(&body);
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
            let mut b = Input::new(&body);
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
        let mut b = Input::new(&body);
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
            let mut b = Input::new(&body);
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

    /// Presents `secret` — 32 bytes only the user has (a passkey's PRF
    /// output, a device's key) — for subject `subject`: the session then
    /// reads and writes the subject's sealed rows whose key the user holds
    /// (PLAN.md §M14.30 K4b).  Sent as its own message, never as a
    /// statement's text; the server keeps it in the session's memory only.
    pub fn present_subject_key(&mut self, subject: &str, secret: &[u8; 32]) -> Result<(), Error> {
        self.subject_key(0, subject, secret)
    }

    /// Gives subject `subject`'s key into its user's keeping, wrapped
    /// under `secret` (an administrator's): from then on only a session
    /// that presents it opens the subject's sealed rows.
    pub fn hold_subject_key(&mut self, subject: &str, secret: &[u8; 32]) -> Result<(), Error> {
        self.subject_key(1, subject, secret)
    }

    fn subject_key(&mut self, action: u8, subject: &str, secret: &[u8; 32]) -> Result<(), Error> {
        if self.version < 8 {
            return Err(Error::Protocol(format!(
                "the server speaks version {} of the protocol; subject keys need 8",
                self.version
            )));
        }
        let mut body = vec![action];
        put_str(&mut body, subject);
        body.extend_from_slice(&32u32.to_le_bytes());
        body.extend_from_slice(secret);
        self.send(b'k', &body)?;
        let mut error = None;
        loop {
            let (kind, body) = self.read()?;
            let mut b = Input::new(&body);
            match kind {
                b'E' => error = Some(b.server_error()?),
                b'R' => {
                    self.set_status(&body);
                    return match error {
                        Some(e) => Err(Error::Server(e)),
                        None => Ok(()),
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

    /// A collection's documents (PLAN.md §M16 Q4): MongoDB's requests —
    /// find, count, insert, update, replace, delete — with filter
    /// documents, each sent as one `O` message and run by the server as
    /// one statement, in this session's transaction.
    pub fn collection(&mut self, name: &str) -> Collection<'_> {
        Collection {
            client: self,
            name: name.to_string(),
        }
    }

    /// A stream's entries (PLAN.md §M16.5 S4): appended, consumed by a
    /// group in this session's transaction, waited for.
    pub fn stream_of(&mut self, name: &str) -> StreamHandle<'_> {
        StreamHandle {
            client: self,
            name: name.to_string(),
        }
    }

    /// A task queue's tasks (`CREATE QUEUE`, PLAN.md §M16.5 S5, S7):
    /// added, taken under a lease, finished with their work, failed.
    pub fn queue_of(&mut self, name: &str) -> QueueHandle<'_> {
        QueueHandle {
            client: self,
            name: name.to_string(),
        }
    }

    /// Subscribes to a query (`SUBSCRIBE`, PLAN.md §M16.6): its result
    /// now, kept by [`Subscription::changes`] as the commits change it.
    /// Its parameters are bound now.  Not inside a transaction.
    pub fn subscribe(&mut self, sql: &str, params: &[Value]) -> Result<Subscription, Error> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let name = format!(
            "nc_live_{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let reply = self.execute(&format!("SUBSCRIBE {name} AS {sql}"), params)?;
        let mut s = Subscription {
            name,
            columns: reply.columns.into_iter().skip(2).collect(),
            rows: Vec::new(),
            at: 0,
        };
        s.apply(reply.rows)?;
        Ok(s)
    }

    /// Waits until stream `stream` has an entry after `after`, or
    /// `timeout_ms` has passed (a `w` message: the server waits holding no
    /// transaction); the stream's newest `seq`.
    pub fn wait_stream(&mut self, stream: &str, after: i64, timeout_ms: u32) -> Result<i64, Error> {
        if self.version < 10 {
            return Err(Error::Protocol(format!(
                "the server speaks version {} of the protocol; waiting on a stream needs 10",
                self.version
            )));
        }
        let mut body = Vec::new();
        put_str(&mut body, stream);
        body.extend_from_slice(&after.to_le_bytes());
        body.extend_from_slice(&timeout_ms.to_le_bytes());
        self.send(b'w', &body)?;
        let reply = match self.replies()?.pop() {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Err(Error::Server(e)),
            None => return Err(Error::Protocol("no result".into())),
        };
        match reply.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n),
            other => Err(Error::Protocol(format!("a stream wait answered {other:?}"))),
        }
    }

    /// Sends a document request (`O`) and reads its one result.
    fn document(&mut self, r: &DocumentRequest<'_>) -> Result<Reply, Error> {
        if self.version < 9 {
            return Err(Error::Protocol(format!(
                "the server speaks version {} of the protocol; document requests need 9",
                self.version
            )));
        }
        let mut body = vec![r.op, u8::from(r.many) | (u8::from(r.upsert) << 1)];
        for s in [r.collection, r.filter, r.body, r.sort] {
            put_str(&mut body, s);
        }
        body.extend_from_slice(&r.skip.to_le_bytes());
        body.extend_from_slice(&r.limit.to_le_bytes());
        self.send(b'O', &body)?;
        match self.replies()?.pop() {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(Error::Server(e)),
            None => Err(Error::Protocol("no result".into())),
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

/// An `O` message's fields.
struct DocumentRequest<'a> {
    op: u8,
    many: bool,
    upsert: bool,
    collection: &'a str,
    filter: &'a str,
    body: &'a str,
    sort: &'a str,
    skip: u64,
    limit: u64,
}

/// A find's options: what of each document (a projection), in which
/// order (a sort document, its fields in its text's order), how many
/// skipped, at most how many (0: all).  JSON texts; empty: none.
#[derive(Clone, Debug, Default)]
pub struct FindOptions {
    /// `{"name": 1, "_id": 0}`, `{"tags": {"$slice": 2}}`.
    pub projection: String,
    /// `{"age": -1, "name": 1}`.
    pub sort: String,
    /// Documents skipped after the sort.
    pub skip: u64,
    /// At most this many (0: no limit).
    pub limit: u64,
}

/// What an update or a replacement did.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateResult {
    /// Documents the filter matched (at most one for `…_one`).
    pub matched: u64,
    /// Of them, those the update changed.
    pub modified: u64,
    /// The `_id` of the document an upsert inserted.
    pub upserted_id: Option<Value>,
}

/// An identifier quoted for SQL.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A live query ([`Client::subscribe`]; PLAN.md §M16.6): its result as
/// of the last batch, kept on this side — the first in the query's order,
/// rows that come after appended, rows gone taken out.
#[derive(Clone, Debug)]
pub struct Subscription {
    name: String,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    at: i64,
}

/// One row's change in a [`Subscription`]'s batch: `diff` copies came
/// (`+n`) or went (`-n`).
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// How many copies of the row came (`+n`) or went (`-n`).
    pub diff: i64,
    /// The row, as the query's columns.
    pub row: Vec<Value>,
}

impl Subscription {
    /// Its name in the session (`CHANGES` and `UNSUBSCRIBE` take it).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The query's columns.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The result as of the last batch.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// The commit the last batch was read at (`nc_ts`).
    pub fn at(&self) -> i64 {
        self.at
    }

    /// The next batch of changes (`CHANGES … WAIT`), applied to
    /// [`rows`](Self::rows): waiting up to `timeout_ms` for a commit that
    /// changes the result; empty when none came in time.  An error ends
    /// the subscription on the server (its table dropped, a privilege
    /// taken back).
    pub fn changes(&mut self, client: &mut Client, timeout_ms: u32) -> Result<Vec<Change>, Error> {
        let reply = client.execute(
            &format!("CHANGES {} WAIT $1", self.name),
            &[Value::Int(i64::from(timeout_ms))],
        )?;
        self.apply(reply.rows)
    }

    /// Ends it (`UNSUBSCRIBE`).
    pub fn close(self, client: &mut Client) -> Result<(), Error> {
        client.execute(&format!("UNSUBSCRIBE {}", self.name), &[])?;
        Ok(())
    }

    /// A batch's rows `(nc_ts, nc_diff, columns…)` applied.
    fn apply(&mut self, rows: Vec<Vec<Value>>) -> Result<Vec<Change>, Error> {
        let mut out = Vec::with_capacity(rows.len());
        for mut r in rows {
            let (Some(Value::Int(at)), Some(Value::Int(diff))) = (r.first(), r.get(1)) else {
                return Err(Error::Protocol(
                    "a live query's row without nc_ts, nc_diff".into(),
                ));
            };
            let (at, diff) = (*at, *diff);
            self.at = at;
            let row = r.split_off(2);
            if diff > 0 {
                for _ in 0..diff {
                    self.rows.push(row.clone());
                }
            } else {
                for _ in 0..-diff {
                    match self.rows.iter().position(|x| *x == row) {
                        Some(i) => {
                            self.rows.remove(i);
                        }
                        None => {
                            return Err(Error::Protocol(
                                "a live query's batch took out a row it did not hold".into(),
                            ));
                        }
                    }
                }
            }
            out.push(Change { diff, row });
        }
        Ok(out)
    }
}

/// A stream's entries, through a [`Client`]'s session (see
/// [`Client::stream_of`]).
pub struct StreamHandle<'c> {
    client: &'c mut Client,
    name: String,
}

impl StreamHandle<'_> {
    /// Appends an entry (an `INSERT`, in the session's transaction): the
    /// values of the columns named; its `seq` and `time` are the commit's.
    pub fn append(&mut self, columns: &[&str], values: &[Value]) -> Result<(), Error> {
        let names: Vec<String> = columns.iter().map(|c| quoted(c)).collect();
        let marks: Vec<String> = (1..=values.len()).map(|i| format!("${i}")).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quoted(&self.name),
            names.join(", "),
            marks.join(", ")
        );
        self.client.execute(&sql, values).map(|_| ())
    }

    /// Group `group`'s next entries, at most `limit` (`CONSUME`): `seq`
    /// first, then the stream's columns.  The group's position moves past
    /// them in the session's transaction — committed with the work done
    /// on them, or not at all.
    pub fn consume(&mut self, group: &str, limit: u32) -> Result<Vec<Vec<Value>>, Error> {
        let sql = format!(
            "CONSUME FROM {} GROUP {} LIMIT $1",
            quoted(&self.name),
            quoted(group)
        );
        self.client.rows(&sql, &[Value::Int(i64::from(limit))])
    }

    /// Group `group`'s position: the last `seq` it consumed.
    pub fn position(&mut self, group: &str) -> Result<i64, Error> {
        let rows = self.client.rows(
            "SELECT stream_position($1, $2)",
            &[Value::Text(self.name.clone()), Value::Text(group.into())],
        )?;
        match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n),
            other => Err(Error::Protocol(format!("stream_position gave {other:?}"))),
        }
    }

    /// [`Client::wait_stream`] on this stream.
    pub fn wait(&mut self, after: i64, timeout_ms: u32) -> Result<i64, Error> {
        self.client.wait_stream(&self.name, after, timeout_ms)
    }

    /// One batch for group `group`, exactly once: in a transaction, its
    /// next entries (at most `limit`, waiting up to `timeout_ms` for one
    /// when there is none) given to `work`, which does what it does in
    /// the same session; then the commit, the group's position with it.
    /// A conflict (`40001`: another consumer of the group took them) runs
    /// it again; an error of `work` rolls all back and is returned.  How
    /// many entries were processed (0: none came in time).  Not in a
    /// transaction of the caller's.
    pub fn process<F>(
        &mut self,
        group: &str,
        limit: u32,
        timeout_ms: u32,
        mut work: F,
    ) -> Result<usize, Error>
    where
        F: FnMut(&mut Client, &[Vec<Value>]) -> Result<(), Error>,
    {
        let conflict = |e: &Error| matches!(e, Error::Server(s) if s.code == "40001");
        let mut waited = false;
        loop {
            self.client.execute("BEGIN", &[])?;
            let rows = match self.consume(group, limit) {
                Ok(rows) => rows,
                Err(e) if conflict(&e) => {
                    self.client.execute("ROLLBACK", &[])?;
                    continue;
                }
                Err(e) => {
                    let _ = self.client.execute("ROLLBACK", &[]);
                    return Err(e);
                }
            };
            if rows.is_empty() {
                let at = self.position(group);
                self.client.execute("ROLLBACK", &[])?;
                if waited || timeout_ms == 0 {
                    return Ok(0);
                }
                self.wait(at?, timeout_ms)?;
                waited = true;
                continue;
            }
            if let Err(e) = work(self.client, &rows) {
                let _ = self.client.execute("ROLLBACK", &[]);
                return Err(e);
            }
            match self.client.execute("COMMIT", &[]) {
                Ok(_) => return Ok(rows.len()),
                Err(e) if conflict(&e) => {
                    if self.client.status() != TxnStatus::Idle {
                        self.client.execute("ROLLBACK", &[])?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// A task a [`QueueHandle::take`] leased: its `id`, the attempt it is
/// (finished or failed by that number), and the queue's columns after its
/// own, in their order.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// The task's id.
    pub id: i64,
    /// Which attempt this is (1 the first).
    pub attempt: i64,
    /// The queue's columns.
    pub values: Vec<Value>,
}

/// A task queue's tasks, through a [`Client`]'s session (see
/// [`Client::queue_of`]).
pub struct QueueHandle<'c> {
    client: &'c mut Client,
    name: String,
}

impl QueueHandle<'_> {
    /// Adds a task (an `INSERT`, in the session's transaction): the values
    /// of the columns named — the queue's own `priority` (lower first) and
    /// `available_at` (a delay) among them if given.  Its id.
    pub fn add(&mut self, columns: &[&str], values: &[Value]) -> Result<i64, Error> {
        let names: Vec<String> = columns.iter().map(|c| quoted(c)).collect();
        let marks: Vec<String> = (1..=values.len()).map(|i| format!("${i}")).collect();
        let sql = if columns.is_empty() {
            format!(
                "INSERT INTO {} DEFAULT VALUES RETURNING id",
                quoted(&self.name)
            )
        } else {
            format!(
                "INSERT INTO {} ({}) VALUES ({}) RETURNING id",
                quoted(&self.name),
                names.join(", "),
                marks.join(", ")
            )
        };
        match self
            .client
            .rows(&sql, values)?
            .first()
            .and_then(|r| r.first())
        {
            Some(Value::Int(id)) => Ok(*id),
            other => Err(Error::Protocol(format!("an added task's id: {other:?}"))),
        }
    }

    /// The next available tasks, at most `limit`, leased for `lease_ms`
    /// (`None`: the queue's lease) — `TAKE`, a transaction of its own
    /// unless the session is in one.
    pub fn take(&mut self, limit: u32, lease_ms: Option<u32>) -> Result<Vec<Task>, Error> {
        let lease = lease_ms
            .map(|ms| format!(" LEASE interval '{ms} milliseconds'"))
            .unwrap_or_default();
        let sql = format!(
            "TAKE FROM {} LIMIT {}{lease}",
            quoted(&self.name),
            limit.max(1)
        );
        self.client
            .rows(&sql, &[])?
            .into_iter()
            .map(|mut r| match (r.first(), r.get(1)) {
                (Some(Value::Int(id)), Some(Value::Int(attempt))) => {
                    let (id, attempt) = (*id, *attempt);
                    Ok(Task {
                        id,
                        attempt,
                        values: r.split_off(2),
                    })
                }
                _ => Err(Error::Protocol(format!("a taken task: {r:?}"))),
            })
            .collect()
    }

    /// Finishes `task` (`FINISH TASK`: deleted, in the session's
    /// transaction — the one its work is in).  `55000` if its attempt no
    /// longer holds it.
    pub fn finish(&mut self, task: &Task) -> Result<(), Error> {
        let sql = format!("FINISH TASK $1 OF {} ATTEMPT $2", quoted(&self.name));
        self.client
            .execute(&sql, &[Value::Int(task.id), Value::Int(task.attempt)])
            .map(|_| ())
    }

    /// Fails `task` (`FAIL TASK`): available again after `retry_after_ms`
    /// or the queue's backoff, or — its last attempt — a dead letter.
    /// `"retry"` or `"dead"`.
    pub fn fail(
        &mut self,
        task: &Task,
        error: Option<&str>,
        retry_after_ms: Option<u32>,
    ) -> Result<String, Error> {
        let mut sql = format!("FAIL TASK $1 OF {} ATTEMPT $2", quoted(&self.name));
        let mut params = vec![Value::Int(task.id), Value::Int(task.attempt)];
        if let Some(e) = error {
            params.push(Value::Text(e.into()));
            sql.push_str(" ERROR $3");
        }
        if let Some(ms) = retry_after_ms {
            sql.push_str(&format!(" RETRY AFTER interval '{ms} milliseconds'"));
        }
        match self
            .client
            .rows(&sql, &params)?
            .first()
            .and_then(|r| r.first())
        {
            Some(Value::Text(outcome)) => Ok(outcome.clone()),
            other => Err(Error::Protocol(format!(
                "a failed task's outcome: {other:?}"
            ))),
        }
    }

    /// Takes up to `limit` tasks (leased for `lease_ms`, `None`: the
    /// queue's) and does each exactly once: in a transaction, `work` on
    /// this session, then `FINISH`, then the commit — the work and the
    /// task's end together.  A task whose work fails is rolled back and
    /// failed with the error's text (retried after the backoff, or a dead
    /// letter); a conflict (`40001`) runs its work again (ten times at
    /// most, then the task is left to its lease), until its finish says
    /// the lease ran out and another took it (`55000`): then it is left to
    /// that one.  How many were finished.  Not in a transaction of the
    /// caller's.
    pub fn process<F>(
        &mut self,
        limit: u32,
        lease_ms: Option<u32>,
        mut work: F,
    ) -> Result<usize, Error>
    where
        F: FnMut(&mut Client, &Task) -> Result<(), Error>,
    {
        let code = |e: &Error| match e {
            Error::Server(s) => s.code.clone(),
            _ => String::new(),
        };
        let mut finished = 0;
        for task in self.take(limit, lease_ms)? {
            for _ in 0..10 {
                self.client.execute("BEGIN", &[])?;
                let ended = work(self.client, &task).and_then(|()| {
                    self.finish(&task)?;
                    self.client.execute("COMMIT", &[]).map(|_| ())
                });
                let Err(e) = ended else {
                    finished += 1;
                    break;
                };
                if self.client.status() != TxnStatus::Idle {
                    self.client.execute("ROLLBACK", &[])?;
                }
                match code(&e).as_str() {
                    "40001" => continue,
                    "55000" => {}
                    _ => match self.fail(&task, Some(&e.to_string()), None) {
                        Ok(_) => {}
                        Err(f) if matches!(code(&f).as_str(), "40001" | "55000") => {}
                        Err(f) => return Err(f),
                    },
                }
                break;
            }
        }
        Ok(finished)
    }
}

/// A collection's documents, through a [`Client`]'s session (see
/// [`Client::collection`]).  Filters, updates, projections, sorts and
/// documents are JSON texts; documents come back as JSON texts.
pub struct Collection<'c> {
    client: &'c mut Client,
    name: String,
}

impl Collection<'_> {
    fn request(
        &mut self,
        op: u8,
        filter: &str,
        body: &str,
        many: bool,
        upsert: bool,
    ) -> Result<Reply, Error> {
        let r = DocumentRequest {
            op,
            many,
            upsert,
            collection: &self.name,
            filter,
            body,
            sort: "",
            skip: 0,
            limit: 0,
        };
        self.client.document(&r)
    }

    /// The documents `filter` matches (`""` or `"{}"`: all).
    pub fn find(&mut self, filter: &str, options: &FindOptions) -> Result<Vec<String>, Error> {
        let r = DocumentRequest {
            op: 1,
            many: false,
            upsert: false,
            collection: &self.name,
            filter,
            body: &options.projection,
            sort: &options.sort,
            skip: options.skip,
            limit: options.limit,
        };
        let reply = self.client.document(&r)?;
        Ok(reply
            .rows
            .into_iter()
            .map(|mut row| json_text(row.swap_remove(0)))
            .collect())
    }

    /// The first document `filter` matches.
    pub fn find_one(&mut self, filter: &str) -> Result<Option<String>, Error> {
        let options = FindOptions {
            limit: 1,
            ..FindOptions::default()
        };
        Ok(self.find(filter, &options)?.pop())
    }

    /// How many documents `filter` matches.
    pub fn count(&mut self, filter: &str) -> Result<u64, Error> {
        let reply = self.request(2, filter, "", false, false)?;
        match reply.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n as u64),
            other => Err(Error::Protocol(format!("a count, not {other:?}"))),
        }
    }

    /// Inserts a document; its `_id` (made where it has none).
    pub fn insert_one(&mut self, doc: &str) -> Result<Value, Error> {
        let mut ids = self.request(3, "", doc, false, false)?.rows;
        ids.pop()
            .and_then(|mut r| r.pop())
            .ok_or_else(|| Error::Protocol("no _id".into()))
    }

    /// Inserts documents, in their order; their `_id`s.
    pub fn insert_many(&mut self, docs: &[&str]) -> Result<Vec<Value>, Error> {
        let body = format!("[{}]", docs.join(", "));
        let rows = self.request(3, "", &body, false, false)?.rows;
        Ok(rows.into_iter().filter_map(|mut r| r.pop()).collect())
    }

    fn updated(
        &mut self,
        op: u8,
        filter: &str,
        body: &str,
        many: bool,
        upsert: bool,
    ) -> Result<UpdateResult, Error> {
        let reply = self.request(op, filter, body, many, upsert)?;
        let row = reply
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| Error::Protocol("no update result".into()))?;
        let n = |v: &Value| match v {
            Value::Int(n) => *n as u64,
            _ => 0,
        };
        Ok(UpdateResult {
            matched: n(&row[0]),
            modified: n(&row[1]),
            upserted_id: (!row[2].is_null()).then(|| row[2].clone()),
        })
    }

    /// Updates the first document `filter` matches by `update`'s
    /// operators (`{"$set": …}`); with `upsert`, inserts one where none
    /// matches.
    pub fn update_one(
        &mut self,
        filter: &str,
        update: &str,
        upsert: bool,
    ) -> Result<UpdateResult, Error> {
        self.updated(4, filter, update, false, upsert)
    }

    /// Updates every document `filter` matches.
    pub fn update_many(
        &mut self,
        filter: &str,
        update: &str,
        upsert: bool,
    ) -> Result<UpdateResult, Error> {
        self.updated(4, filter, update, true, upsert)
    }

    /// Replaces the first document `filter` matches (its `_id` kept).
    pub fn replace_one(
        &mut self,
        filter: &str,
        doc: &str,
        upsert: bool,
    ) -> Result<UpdateResult, Error> {
        self.updated(5, filter, doc, false, upsert)
    }

    /// Deletes the first document `filter` matches; how many (0 or 1).
    pub fn delete_one(&mut self, filter: &str) -> Result<u64, Error> {
        Ok(self.request(6, filter, "", false, false)?.count)
    }

    /// Deletes every document `filter` matches; how many.
    pub fn delete_many(&mut self, filter: &str) -> Result<u64, Error> {
        Ok(self.request(6, filter, "", true, false)?.count)
    }
}

/// A `json` value's text.
fn json_text(v: Value) -> String {
    match v {
        Value::Json(s) | Value::Text(s) => s,
        other => other.to_string(),
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
            let mut b = Input::new(&body);
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
            .map(|body| body.and_then(|b| Input::new(&b).row(n)))
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
            let mut b = Input::new(&body);
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
    let pad = |b: &mut Vec<u8>| columnar::pad(b, 0);
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
                    (Value::Json(text), Type::Json) => data.extend_from_slice(text.as_bytes()),
                    (Value::Vector(v), Type::Vector) => data.extend_from_slice(&vector_bytes(v)),
                    (
                        Value::SparseVector {
                            dims,
                            indices,
                            values,
                        },
                        Type::SparseVector,
                    ) => data.extend_from_slice(&sparse_bytes(*dims, indices, values)),
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
        Type::Uuid => columnar::width(tag::EXT, Some("uuid")),
        t => columnar::width(tag_of_type(t), None),
    }
}

/// A column type's tag (its own type's, for the columnar layout).
fn tag_of_type(t: &Type) -> u8 {
    match t {
        Type::Bool => tag::BOOL,
        Type::SmallInt => tag::SMALLINT,
        Type::Integer => tag::INTEGER,
        Type::Int => tag::BIGINT,
        Type::Real => tag::REAL,
        Type::Float => tag::DOUBLE,
        Type::Numeric => tag::NUMERIC,
        Type::Date => tag::DATE,
        Type::Timestamp => tag::TIMESTAMP,
        Type::TimestampTz => tag::TIMESTAMPTZ,
        Type::Time => tag::TIME,
        Type::Interval => tag::INTERVAL,
        Type::Bytea => tag::BYTEA,
        Type::Inet => tag::INET,
        Type::Cidr => tag::CIDR,
        Type::Char => tag::CHAR,
        Type::Array(_) => tag::ARRAY,
        Type::Uuid | Type::Vector | Type::SparseVector | Type::Json | Type::Other(_) => tag::EXT,
        Type::Enum(_) => tag::ENUM,
        Type::Text | Type::Unknown => tag::TEXT,
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
        let align = |at: &mut usize| *at = columnar::align(*at);
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
                    Type::Vector => Value::Vector(vector_of(b)),
                    Type::Json => Value::Json(
                        String::from_utf8(b.to_vec())
                            .map_err(|_| Error::Protocol("json is not UTF-8".into()))?,
                    ),
                    Type::SparseVector => sparse_of(b).unwrap_or_else(|| Value::Other {
                        type_name: "sparsevec".into(),
                        bytes: b.to_vec(),
                    }),
                    Type::Inet | Type::Cidr => {
                        // The row format's payload, behind its tag.
                        let mut tagged = vec![if *t == Type::Inet { 18 } else { 19 }];
                        tagged.extend_from_slice(b);
                        Value::decode(&mut Input::new(&tagged))?
                    }
                    Type::Array(_) => Value::decode(&mut Input::new(b))?,
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
