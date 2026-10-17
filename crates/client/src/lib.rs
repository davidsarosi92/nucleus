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
//! # Ok::<(), nucleus_client::Error>(())
//! ```

use std::fmt;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The protocol version this driver speaks (2: registered types such as
/// `uuid` arrive in their binary form).
pub const PROTOCOL_VERSION: u16 = 2;
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
        }
    }
}

/// An error the server reported: SQLSTATE and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError {
    /// SQLSTATE (5 characters).
    pub code: String,
    /// The message.
    pub message: String,
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Server(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
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
    /// `bigint`.
    Int,
    /// `double precision`.
    Float,
    /// `text`.
    Text,
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
    /// A 64-bit integer.
    Int(i64),
    /// A double.
    Float(f64),
    /// Text.
    Text(String),
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
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) => f.write_str(s),
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
    input: BufReader<TcpStream>,
    output: BufWriter<TcpStream>,
    status: TxnStatus,
    server: String,
    session: u32,
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
        let addr = (config.host.as_str(), config.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::Protocol(format!("cannot resolve {}", config.host)))?;
        let stream = TcpStream::connect_timeout(&addr, config.connect_timeout)?;
        stream.set_nodelay(true)?;
        let mut c = Client {
            input: BufReader::new(stream.try_clone()?),
            output: BufWriter::new(stream),
            status: TxnStatus::Idle,
            server: String::new(),
            session: 0,
            config,
        };
        let mut hello = PROTOCOL_VERSION.to_le_bytes().to_vec();
        for s in [
            &c.config.user,
            &c.config.database,
            c.config.password.as_deref().unwrap_or(""),
            &c.config.client_name,
        ] {
            put_str(&mut hello, s);
        }
        c.output.write_all(&frame(b'H', &hello))?;
        c.output.flush()?;
        loop {
            let (kind, body) = c.read()?;
            let mut b = Input { b: &body };
            match kind {
                b'W' => {
                    let _version = b.u16()?;
                    c.server = b.str()?;
                    c.session = b.u32()?;
                }
                b'R' => {
                    c.set_status(&body);
                    return Ok(c);
                }
                b'E' => {
                    return Err(Error::Server(ServerError {
                        code: b.str()?,
                        message: b.str()?,
                    }));
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

    fn read(&mut self) -> Result<(u8, Vec<u8>), Error> {
        let mut h = [0u8; 5];
        self.input.read_exact(&mut h)?;
        let len = u32::from_le_bytes(h[1..5].try_into().unwrap());
        if len > 1 << 30 {
            return Err(Error::Protocol("message too large".into()));
        }
        let mut body = vec![0u8; len as usize];
        self.input.read_exact(&mut body)?;
        Ok((h[0], body))
    }

    fn set_status(&mut self, body: &[u8]) {
        self.status = match body.first() {
            Some(1) => TxnStatus::InBlock,
            Some(2) => TxnStatus::Failed,
            _ => TxnStatus::Idle,
        };
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
        let mut body = Vec::new();
        put_str(&mut body, sql);
        body.push(u8::from(format == Format::Text));
        body.extend_from_slice(&(params.len() as u16).to_le_bytes());
        for p in params {
            p.encode(&mut body);
        }
        self.output.write_all(&frame(b'Q', &body))?;
        self.output.flush()?;
        let mut out = Vec::new();
        let mut current: Option<(Vec<Column>, Vec<Vec<Value>>)> = None;
        loop {
            let (kind, body) = self.read()?;
            let mut b = Input { b: &body };
            match kind {
                b'C' => {
                    let n = b.u16()? as usize;
                    let mut columns = Vec::with_capacity(n);
                    for _ in 0..n {
                        let name = b.str()?;
                        let tag = b.u8()?;
                        let ty = match tag {
                            10 => Type::Array(Box::new(type_of_tag(b.u8()?))),
                            11 => match b.str()? {
                                n if n == "uuid" => Type::Uuid,
                                n => Type::Other(n),
                            },
                            _ => type_of_tag(tag),
                        };
                        columns.push(Column { name, ty });
                    }
                    current = Some((columns, Vec::new()));
                }
                b'D' => {
                    let (columns, rows) = current
                        .as_mut()
                        .ok_or_else(|| Error::Protocol("a row before its columns".into()))?;
                    let mut row = Vec::with_capacity(columns.len());
                    for _ in 0..columns.len() {
                        row.push(Value::decode(&mut b)?);
                    }
                    rows.push(row);
                }
                b'K' => {
                    let command = b.str()?;
                    let count = u64::from_le_bytes(b.take(8)?.try_into().unwrap());
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
                    out.push(Err(ServerError {
                        code: b.str()?,
                        message: b.str()?,
                    }));
                }
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
        self.output.write_all(&frame(b'X', &[]))?;
        self.output.flush()
    }
}
