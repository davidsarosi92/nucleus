//! The nucleus protocol (`ncp`, DOCS §10): one connection.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_sql::encoding::{Reader, decode_value, encode_value};
use nucleus_sql::ext::TypeRef;

/// The value and type tag of a registered type (version 2).
const EXT: u8 = 11;
use nucleus_sql::{DataType, Error, Output, Session, TxnStatus, Value};

use crate::Config;

/// The newest protocol version this server speaks.  Version 2 adds
/// registered types (value and type tag 11: name and binary form); a
/// version-1 client gets them as text.
pub const VERSION: u16 = 2;
/// The oldest it still speaks.
pub const MIN_VERSION: u16 = 1;
const MAX_BODY: u32 = 1 << 30;

/// An outgoing message.
struct Msg {
    buf: Vec<u8>,
}

impl Msg {
    fn new(kind: u8) -> Self {
        Self {
            buf: vec![kind, 0, 0, 0, 0],
        }
    }
    fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }
    fn u16(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn str(mut self, s: &str) -> Self {
        self = self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }
    fn ty(mut self, t: Option<DataType>, v2: bool) -> Self {
        if let (Some(DataType::Ext(t)), true) = (t, v2) {
            return self.u8(EXT).str(t.name());
        }
        let tag = |t: DataType| match t {
            DataType::Bool => 1,
            DataType::SmallInt | DataType::Integer | DataType::BigInt => 2,
            DataType::Float => 3,
            DataType::Text => 4,
            DataType::Numeric => 5,
            DataType::Date => 6,
            DataType::Timestamp => 7,
            DataType::TimestampTz => 8,
            DataType::Interval => 9,
            DataType::Array(_) => 10,
            // Registered types travel as text to version-1 clients.
            DataType::Ext(_) => 4,
        };
        match t {
            None => self.u8(0),
            Some(t @ DataType::Array(e)) => {
                self = self.u8(tag(t));
                self.u8(tag(*e))
            }
            Some(t) => self.u8(tag(t)),
        }
    }
    fn value(mut self, v: &Value, v2: bool) -> Self {
        wire_value(&mut self.buf, v, v2);
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let len = (self.buf.len() - 5) as u32;
        self.buf[1..5].copy_from_slice(&len.to_le_bytes());
        self.buf
    }
}

/// Appends a value in the protocol's encoding (DOCS §10).  It shares its
/// tags with the row encoding's version 1, but is the protocol's own: an
/// integer is always 8 bytes, whatever the storage does.
fn wire_value(buf: &mut Vec<u8>, v: &Value, v2: bool) {
    match v {
        Value::Int(i) => {
            buf.push(2);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Array(items) => {
            buf.push(10);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                wire_value(buf, item, v2);
            }
        }
        Value::Ext(t, bytes) if v2 => {
            buf.push(EXT);
            buf.extend_from_slice(&(t.name().len() as u32).to_le_bytes());
            buf.extend_from_slice(t.name().as_bytes());
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        // Registered types travel as text to version-1 clients.
        Value::Ext(..) => encode_value(buf, &Value::Text(v.render())),
        v => encode_value(buf, v),
    }
}

fn protocol(what: impl Into<String>) -> Error {
    Error {
        code: "08P01",
        message: what.into(),
    }
}

fn read_message(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
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

/// Reads the fields of a client message.
struct Body<'a> {
    b: &'a [u8],
}

impl<'a> Body<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.b.len() < n {
            return Err(protocol("message too short"));
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
    fn str(&mut self) -> Result<String, Error> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| protocol("string is not UTF-8"))
    }
}

fn error_msg(e: &Error) -> Vec<u8> {
    Msg::new(b'E').str(e.code).str(&e.message).finish()
}

fn ready_msg(s: TxnStatus) -> Vec<u8> {
    Msg::new(b'R')
        .u8(match s {
            TxnStatus::Idle => 0,
            TxnStatus::InBlock => 1,
            TxnStatus::Failed => 2,
        })
        .finish()
}

/// `INSERT 0 3` → (`INSERT`, 3); `CREATE TABLE` → (`CREATE TABLE`, 0).
fn split_tag(tag: &str) -> (String, u64) {
    let words: Vec<&str> = tag.split_whitespace().collect();
    match words.last().and_then(|w| w.parse::<u64>().ok()) {
        Some(n) => {
            let cmd: Vec<&str> = words[..words.len() - 1]
                .iter()
                .copied()
                .filter(|w| w.parse::<u64>().is_err())
                .collect();
            (cmd.join(" "), n)
        }
        None => (tag.to_string(), 0),
    }
}

/// Serves one client until it says Bye or goes away.
pub fn handle(stream: TcpStream, kv: Arc<dyn Kv>, config: &Config, id: u32) -> io::Result<()> {
    let mut input = BufReader::new(stream.try_clone()?);
    let mut out = BufWriter::new(stream);
    let log = |what: &str, text: &str| {
        if config.log_queries {
            eprintln!("nucleus-server: [{id}] {what}: {}", text.trim());
        }
    };

    // Hello.
    let (kind, body) = read_message(&mut input)?;
    let hello = (|| -> Result<u16, Error> {
        if kind != b'H' {
            return Err(protocol("expected Hello"));
        }
        let mut b = Body { b: &body };
        let version = b.u16()?;
        if version < MIN_VERSION {
            return Err(protocol(format!(
                "unsupported protocol version {version} (this server speaks \
                 {MIN_VERSION} to {VERSION})"
            )));
        }
        let user = b.str()?;
        let _database = b.str()?;
        let password = b.str()?;
        let client = b.str()?;
        log("hello", &format!("user {user}, client {client}"));
        if !config.accepts(&user, &password) {
            return Err(Error {
                code: "28P01",
                message: format!("password authentication failed for user \"{user}\""),
            });
        }
        Ok(version.min(VERSION))
    })();
    let version = match hello {
        Ok(v) => v,
        Err(e) => {
            out.write_all(&error_msg(&e))?;
            return out.flush();
        }
    };
    out.write_all(
        &Msg::new(b'W')
            .u16(version)
            .str(&format!("nucleus {}", env!("CARGO_PKG_VERSION")))
            .u32(id)
            .finish(),
    )?;
    let mut session = Session::with_registry(kv, config.registry.clone());
    out.write_all(&ready_msg(session.status()))?;
    out.flush()?;

    loop {
        let (kind, body) = read_message(&mut input)?;
        match kind {
            b'Q' => {
                let parsed = (|| -> Result<(String, u8, Vec<Value>), Error> {
                    let mut b = Body { b: &body };
                    let sql = b.str()?;
                    let format = b.u8()?;
                    let n = b.u16()? as usize;
                    let mut r = Reader::new(b.b);
                    let mut params = Vec::with_capacity(n);
                    for _ in 0..n {
                        params.push(parameter(&mut r)?);
                    }
                    Ok((sql, format, params))
                })();
                match parsed {
                    Ok((sql, format, params)) => {
                        log("query", &sql);
                        let v2 = version >= 2;
                        if let Err(e) = run(&mut session, &sql, format == 1, v2, &params, &mut out)?
                        {
                            log("error", &format!("{} {}", e.code, e.message));
                            out.write_all(&error_msg(&e))?;
                        }
                    }
                    Err(e) => out.write_all(&error_msg(&e))?,
                }
                out.write_all(&ready_msg(session.status()))?;
                out.flush()?;
            }
            b'X' => return Ok(()),
            other => {
                out.write_all(&error_msg(&protocol(format!(
                    "unexpected message {:?}",
                    other as char
                ))))?;
                return out.flush();
            }
        }
    }
}

/// Runs a Query's statements, writing their results; the first error is
/// returned (not yet written).
/// A parameter: a value in the row encoding, or (tag 11) a registered
/// type's name and binary form.
fn parameter(r: &mut Reader) -> Result<Value, Error> {
    let bad = |_| protocol("malformed parameter");
    if r.peek() != Some(EXT) {
        return decode_value(r).map_err(bad);
    }
    r.u8().map_err(bad)?;
    let name = r.str().map_err(bad)?;
    let n = r.u32().map_err(bad)? as usize;
    let bytes = r.take(n).map_err(bad)?;
    let t = TypeRef::named(&name).ok_or_else(|| Error {
        code: "42704",
        message: format!("type \"{name}\" does not exist"),
    })?;
    // The binary form is the canonical one; checked, as any input is.
    let v = Value::ext(t, bytes.to_vec());
    t.imp()
        .input(&t.imp().output(&v))
        .map_err(|_| protocol("malformed parameter"))?;
    Ok(v)
}

fn run(
    session: &mut Session,
    sql: &str,
    text: bool,
    v2: bool,
    params: &[Value],
    out: &mut impl Write,
) -> io::Result<Result<(), Error>> {
    let stmts = match nucleus_sql::parser::parse(sql) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    if !params.is_empty() && stmts.len() > 1 {
        return Ok(Err(Error::syntax(
            "a query with parameters must hold one statement",
        )));
    }
    // Parameters take the types their places need (text is parsed).
    let mut converted = params.to_vec();
    if let [stmt] = stmts.as_slice()
        && !params.is_empty()
    {
        let types = match session.describe(stmt) {
            Ok((types, _)) => types,
            Err(e) => return Ok(Err(e)),
        };
        for (v, t) in converted.iter_mut().zip(types) {
            if let Some(t) = t
                && !v.is_null()
                && v.data_type() != Some(t)
            {
                match v.clone().cast(t) {
                    Ok(c) => *v = c,
                    Err(e) => return Ok(Err(e)),
                }
            }
        }
    }
    let params = converted.as_slice();
    for stmt in &stmts {
        let output = match session.run(stmt, params) {
            Ok(o) => o,
            Err(e) => return Ok(Err(e)),
        };
        let (command, count) = split_tag(&nucleus_sql::command_tag(stmt, &output));
        if let Output::Rows { columns, rows } = output {
            let mut m = Msg::new(b'C').u16(columns.len() as u16);
            for c in &columns {
                m = m.str(&c.name).ty(Some(c.ty), v2);
            }
            out.write_all(&m.finish())?;
            for row in &rows {
                let mut m = Msg::new(b'D');
                for v in row {
                    m = if text && !v.is_null() {
                        m.value(&Value::Text(v.render()), v2)
                    } else {
                        m.value(v, v2)
                    };
                }
                out.write_all(&m.finish())?;
            }
        }
        out.write_all(&Msg::new(b'K').str(&command).u64(count).finish())?;
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::split_tag;

    #[test]
    fn tags() {
        assert_eq!(split_tag("INSERT 0 3"), ("INSERT".into(), 3));
        assert_eq!(split_tag("UPDATE 2"), ("UPDATE".into(), 2));
        assert_eq!(split_tag("SELECT 0"), ("SELECT".into(), 0));
        assert_eq!(split_tag("CREATE TABLE"), ("CREATE TABLE".into(), 0));
    }
}
