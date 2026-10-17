//! A client for nucleus servers (PLAN.md §M14.6): the frontend side of the
//! PostgreSQL wire protocol (version 3), the protocol `nucleus server`
//! speaks.
//!
//! [`Client::connect`] opens a connection (trust or cleartext-password
//! authentication; no TLS), [`Client::query`] runs one or more statements
//! with the simple query protocol and returns each statement's result as
//! text.  It also talks to PostgreSQL servers configured for trust or
//! password authentication.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PROTOCOL_V3: i32 = 196_608;

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
    /// Password, if the server asks for one.
    pub password: Option<String>,
    /// Reported as `application_name`.
    pub application_name: String,
    /// How long to wait for the connection.
    pub connect_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 5432,
            user: "nucleus".into(),
            database: "nucleus".into(),
            password: None,
            application_name: "ncsql".into(),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// An error the server reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError {
    /// `ERROR`, `FATAL`, …
    pub severity: String,
    /// SQLSTATE.
    pub code: String,
    /// The message.
    pub message: String,
    /// The detail line, if any.
    pub detail: Option<String>,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:  {} ({})", self.severity, self.message, self.code)?;
        if let Some(d) = &self.detail {
            write!(f, "\nDETAIL:  {d}")?;
        }
        Ok(())
    }
}

/// A connection-level error.
#[derive(Debug)]
pub enum Error {
    /// The network.
    Io(io::Error),
    /// The server refused (authentication, unknown database, …).
    Server(ServerError),
    /// Something the client does not understand.
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

/// A result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Name.
    pub name: String,
    /// The type's oid.
    pub type_oid: i32,
}

/// One statement's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Rows (values as text, `None` for NULL) and the command tag.
    Rows {
        /// The columns.
        columns: Vec<Column>,
        /// The rows.
        rows: Vec<Vec<Option<String>>>,
        /// `SELECT 3`, `INSERT 0 1` (with `RETURNING`), …
        tag: String,
    },
    /// A command: its tag (`CREATE TABLE`, `INSERT 0 3`, …).
    Command(String),
    /// An empty query string.
    Empty,
}

/// The transaction status the server reports after each query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// Not in a transaction block.
    Idle,
    /// In a transaction block.
    InBlock,
    /// In a failed transaction block.
    Failed,
}

/// A connection.
pub struct Client {
    input: BufReader<TcpStream>,
    output: BufWriter<TcpStream>,
    status: TxnStatus,
    parameters: HashMap<String, String>,
    config: Config,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

fn bad(what: &str) -> Error {
    Error::Protocol(what.into())
}

/// The fields of an ErrorResponse or NoticeResponse.
fn fields(body: &[u8]) -> ServerError {
    let mut e = ServerError {
        severity: "ERROR".into(),
        code: String::new(),
        message: String::new(),
        detail: None,
    };
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let kind = body[i];
        let end = body[i + 1..]
            .iter()
            .position(|&c| c == 0)
            .map_or(body.len(), |p| i + 1 + p);
        let v = String::from_utf8_lossy(&body[i + 1..end]).into_owned();
        match kind {
            b'S' => e.severity = v,
            b'C' => e.code = v,
            b'M' => e.message = v,
            b'D' => e.detail = Some(v),
            _ => {}
        }
        i = end + 1;
    }
    e
}

fn int16(b: &[u8], at: usize) -> Result<i16, Error> {
    Ok(i16::from_be_bytes(
        b.get(at..at + 2)
            .ok_or_else(|| bad("short message"))?
            .try_into()
            .unwrap(),
    ))
}

fn int32(b: &[u8], at: usize) -> Result<i32, Error> {
    Ok(i32::from_be_bytes(
        b.get(at..at + 4)
            .ok_or_else(|| bad("short message"))?
            .try_into()
            .unwrap(),
    ))
}

impl Client {
    /// Connects and authenticates.
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
            parameters: HashMap::new(),
            config,
        };
        c.startup()?;
        Ok(c)
    }

    fn startup(&mut self) -> Result<(), Error> {
        let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
        for (k, v) in [
            ("user", self.config.user.clone()),
            ("database", self.config.database.clone()),
            ("application_name", self.config.application_name.clone()),
            ("client_encoding", "UTF8".into()),
        ] {
            cstr(&mut body, k);
            cstr(&mut body, &v);
        }
        body.push(0);
        let mut m = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        m.extend(body);
        self.output.write_all(&m)?;
        self.output.flush()?;
        loop {
            let (tag, body) = self.read()?;
            match tag {
                b'R' => match int32(&body, 0)? {
                    0 => {}
                    3 => {
                        let Some(pw) = self.config.password.clone() else {
                            return Err(Error::Protocol("the server wants a password".into()));
                        };
                        let mut p = Vec::new();
                        cstr(&mut p, &pw);
                        self.output.write_all(&message(b'p', &p))?;
                        self.output.flush()?;
                    }
                    other => {
                        return Err(Error::Protocol(format!(
                            "unsupported authentication method {other} (trust and password are)"
                        )));
                    }
                },
                b'S' => {
                    let mut parts = body.split(|&c| c == 0);
                    let k = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                    let v = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                    self.parameters.insert(k, v);
                }
                b'K' | b'N' => {}
                b'E' => return Err(Error::Server(fields(&body))),
                b'Z' => {
                    self.set_status(&body);
                    return Ok(());
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        other as char
                    )));
                }
            }
        }
    }

    fn read(&mut self) -> Result<(u8, Vec<u8>), Error> {
        let mut h = [0u8; 5];
        self.input.read_exact(&mut h)?;
        let len = i32::from_be_bytes(h[1..5].try_into().unwrap());
        if !(4..=1 << 30).contains(&len) {
            return Err(bad("bad message length"));
        }
        let mut body = vec![0u8; len as usize - 4];
        self.input.read_exact(&mut body)?;
        Ok((h[0], body))
    }

    fn set_status(&mut self, body: &[u8]) {
        self.status = match body.first() {
            Some(b'T') => TxnStatus::InBlock,
            Some(b'E') => TxnStatus::Failed,
            _ => TxnStatus::Idle,
        };
    }

    /// Runs `sql` (one or more `;`-separated statements) with the simple
    /// query protocol.  Returns each statement's reply; a failing
    /// statement ends the list with its error (the server skips the rest).
    pub fn query(&mut self, sql: &str) -> Result<Vec<Result<Reply, ServerError>>, Error> {
        let mut q = Vec::new();
        cstr(&mut q, sql);
        self.output.write_all(&message(b'Q', &q))?;
        self.output.flush()?;
        let mut out = Vec::new();
        let mut current: Option<(Vec<Column>, Vec<Vec<Option<String>>>)> = None;
        loop {
            let (tag, body) = self.read()?;
            match tag {
                b'T' => {
                    let n = int16(&body, 0)? as usize;
                    let mut at = 2;
                    let mut columns = Vec::with_capacity(n);
                    for _ in 0..n {
                        let end = body[at..]
                            .iter()
                            .position(|&c| c == 0)
                            .ok_or_else(|| bad("unterminated column name"))?;
                        let name = String::from_utf8_lossy(&body[at..at + end]).into_owned();
                        at += end + 1;
                        let type_oid = int32(&body, at + 6)?;
                        at += 18;
                        columns.push(Column { name, type_oid });
                    }
                    current = Some((columns, Vec::new()));
                }
                b'D' => {
                    let n = int16(&body, 0)? as usize;
                    let mut at = 2;
                    let mut row = Vec::with_capacity(n);
                    for _ in 0..n {
                        let len = int32(&body, at)?;
                        at += 4;
                        if len < 0 {
                            row.push(None);
                        } else {
                            let v = body
                                .get(at..at + len as usize)
                                .ok_or_else(|| bad("short data row"))?;
                            row.push(Some(String::from_utf8_lossy(v).into_owned()));
                            at += len as usize;
                        }
                    }
                    current
                        .get_or_insert_with(|| (Vec::new(), Vec::new()))
                        .1
                        .push(row);
                }
                b'C' => {
                    let tag =
                        String::from_utf8_lossy(body.split(|&c| c == 0).next().unwrap_or_default())
                            .into_owned();
                    out.push(Ok(match current.take() {
                        Some((columns, rows)) => Reply::Rows { columns, rows, tag },
                        None => Reply::Command(tag),
                    }));
                }
                b'I' => out.push(Ok(Reply::Empty)),
                b'E' => {
                    current = None;
                    out.push(Err(fields(&body)));
                }
                b'N' | b'S' | b'A' => {}
                b'Z' => {
                    self.set_status(&body);
                    return Ok(out);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?}",
                        other as char
                    )));
                }
            }
        }
    }

    /// The transaction status after the last query.
    pub fn status(&self) -> TxnStatus {
        self.status
    }

    /// A parameter the server reported (`server_version`, …).
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(String::as_str)
    }

    /// The configuration it connected with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Ends the session.
    pub fn close(mut self) -> io::Result<()> {
        self.output.write_all(&message(b'X', &[]))?;
        self.output.flush()
    }
}
