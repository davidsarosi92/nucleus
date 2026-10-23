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
/// version-1 client gets them as text.  Version 3 adds tags 12–20
/// (`smallint`, `integer`, `real`, `bytea`, `time`, `char(n)`, `inet`,
/// `cidr`, enums); an older client gets them as before — integers as
/// `bigint`, a real as a double, the rest as text.  Version 4 logs in by
/// SCRAM-SHA-256 (`A` / `a` messages), so a password never crosses the
/// wire; older clients still send theirs in clear.
pub const VERSION: u16 = 4;
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
    fn ty(mut self, t: Option<DataType>, version: u16) -> Self {
        match t {
            Some(DataType::Ext(t)) if version >= 2 => self.u8(EXT).str(t.name()),
            Some(DataType::Enum(e)) if version >= 3 => self.u8(ENUM).str(e.name()),
            None => self.u8(0),
            Some(t @ DataType::Array(e)) => {
                self = self.u8(type_tag(t, version));
                self.u8(type_tag(*e, version))
            }
            Some(t) => self.u8(type_tag(t, version)),
        }
    }
    fn value(mut self, v: &Value, ty: Option<DataType>, version: u16) -> Self {
        wire_value(&mut self.buf, v, ty, version);
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let len = (self.buf.len() - 5) as u32;
        self.buf[1..5].copy_from_slice(&len.to_le_bytes());
        self.buf
    }
}

/// An enum column's type tag (version 3): its name follows.
const ENUM: u8 = 20;

/// A column type's tag in protocol `version` (DOCS §10).
fn type_tag(t: DataType, version: u16) -> u8 {
    let v3 = version >= 3;
    match t {
        DataType::Bool => 1,
        DataType::SmallInt if v3 => 12,
        DataType::Integer if v3 => 13,
        DataType::SmallInt | DataType::Integer | DataType::BigInt => 2,
        DataType::Real if v3 => 14,
        DataType::Real | DataType::Float => 3,
        DataType::Bytea if v3 => 15,
        DataType::Time if v3 => 16,
        DataType::Char if v3 => 17,
        DataType::Inet if v3 => 18,
        DataType::Cidr if v3 => 19,
        DataType::Text
        | DataType::Char
        | DataType::Bytea
        | DataType::Time
        | DataType::Inet
        | DataType::Cidr => 4,
        DataType::Numeric => 5,
        DataType::Date => 6,
        DataType::Timestamp => 7,
        DataType::TimestampTz => 8,
        DataType::Interval => 9,
        DataType::Array(_) => 10,
        // Registered types (to version-1 clients) and enums (before
        // version 3) travel as text.
        DataType::Ext(_) | DataType::Enum(_) => 4,
    }
}

/// Appends a value of column type `ty` in the protocol's encoding (DOCS
/// §10).  Its tags 0–10 are the row encoding's version 1, but it is the
/// protocol's own: the storage may store a value more compactly without
/// the protocol changing.
fn wire_value(buf: &mut Vec<u8>, v: &Value, ty: Option<DataType>, version: u16) {
    let v3 = version >= 3;
    match v {
        Value::Int(i) => match ty {
            Some(DataType::SmallInt) if v3 => {
                buf.push(12);
                buf.extend_from_slice(&(*i as i16).to_le_bytes());
            }
            Some(DataType::Integer) if v3 => {
                buf.push(13);
                buf.extend_from_slice(&(*i as i32).to_le_bytes());
            }
            _ => {
                buf.push(2);
                buf.extend_from_slice(&i.to_le_bytes());
            }
        },
        Value::Real(f) if v3 => {
            buf.push(14);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(3);
            buf.extend_from_slice(&f64::from(*f).to_le_bytes());
        }
        Value::Bytea(b) if v3 => {
            buf.push(15);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Time(t) if v3 => {
            buf.push(16);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Inet(n) | Value::Cidr(n) if v3 => {
            buf.push(if matches!(v, Value::Inet(_)) { 18 } else { 19 });
            buf.extend(n.to_bytes());
        }
        // char(n) and enum values are text (their column types say what
        // they are); older clients get the others as text too.
        Value::Bytea(_)
        | Value::Time(_)
        | Value::Char(_)
        | Value::Inet(_)
        | Value::Cidr(_)
        | Value::Enum(_) => encode_value(buf, &Value::Text(v.render())),
        Value::Array(items) => {
            buf.push(10);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            let elem = ty.and_then(DataType::element);
            for item in items {
                wire_value(buf, item, elem, version);
            }
        }
        Value::Ext(t, bytes) if version >= 2 => {
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

fn read_message<R: Read + ?Sized>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
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
    let log = |what: &str, text: &str| {
        if config.log_queries {
            eprintln!("nucleus-server: [{id}] {what}: {}", text.trim());
        }
    };
    // A client that starts with a TLS handshake asks for TLS (§10).
    let mut first = [0u8; 1];
    if stream.peek(&mut first)? == 0 {
        return Ok(());
    }
    let stream: Box<dyn crate::Stream> = if first[0] == crate::transport::TLS_HANDSHAKE {
        let Some(tls) = &config.tls else {
            log("tls", "refused: this server has no TLS configured");
            return Ok(());
        };
        let secured = tls.accept(stream)?;
        log(
            "tls",
            &format!("{} {}", secured.info.version, secured.info.cipher),
        );
        secured.stream
    } else {
        Box::new(stream)
    };
    let (r, w) = crate::transport::split(stream);
    let mut input = BufReader::new(r);
    let mut out = BufWriter::new(w);

    // Hello, and the login it starts.
    let hello = login(&mut input, &mut out, &kv, config, &log)?;
    let (version, mut session) = match hello {
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
                        if let Err(e) =
                            run(&mut session, &sql, format == 1, version, &params, &mut out)?
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

/// Reads Hello and logs the client in: by SCRAM-SHA-256 from protocol
/// version 4, else by the password Hello carries.  The session, or the
/// error to send.
fn login(
    input: &mut impl Read,
    out: &mut impl Write,
    kv: &Arc<dyn Kv>,
    config: &Config,
    log: &dyn Fn(&str, &str),
) -> io::Result<Result<(u16, Session), Error>> {
    let (kind, body) = read_message(input)?;
    let parsed = (|| -> Result<(u16, String, String), Error> {
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
        Ok((version.min(VERSION), user, password))
    })();
    let (version, user, password) = match parsed {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let refused = || Error {
        code: "28P01",
        message: format!("password authentication failed for user \"{user}\""),
    };
    if config.auth.wants_password() {
        if version >= 4 {
            match scram(input, out, &**kv, config, &user)? {
                Ok(true) => {}
                Ok(false) => return Ok(Err(refused())),
                Err(e) => return Ok(Err(e)),
            }
        } else if !config.accepts(&**kv, &user, &password) {
            return Ok(Err(refused()));
        }
    }
    let mut session = Session::with_registry(kv.clone(), config.registry.clone());
    Ok(match session.login(&user) {
        Ok(()) => Ok((version, session)),
        Err(e) => Err(e),
    })
}

/// The SCRAM-SHA-256 exchange: `A` names the mechanism, then `a` (client
/// first) / `A` (server first) / `a` (client final) / `A` (server final).
/// Whether the client proved its password.
fn scram(
    input: &mut impl Read,
    out: &mut impl Write,
    kv: &dyn Kv,
    config: &Config,
    user: &str,
) -> io::Result<Result<bool, Error>> {
    let mut server = crate::scram_server(config, kv, user);
    let send = |out: &mut dyn Write, s: &str| -> io::Result<()> {
        out.write_all(&Msg::new(b'A').str(s).finish())?;
        out.flush()
    };
    let receive = |input: &mut dyn Read| -> io::Result<Result<String, Error>> {
        let (kind, body) = read_message(input)?;
        if kind != b'a' {
            return Ok(Err(protocol("expected an authentication response")));
        }
        Ok(Body { b: &body }.str())
    };
    send(out, "SCRAM-SHA-256")?;
    let first = match receive(input)? {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    let Ok(server_first) = server.first(&first) else {
        return Ok(Err(protocol("malformed SCRAM message")));
    };
    send(out, &server_first)?;
    let last = match receive(input)? {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    match server.finish(&last) {
        Ok(server_final) => {
            send(out, &server_final)?;
            Ok(Ok(true))
        }
        Err(nucleus_crypto::ScramError::Rejected) => Ok(Ok(false)),
        Err(nucleus_crypto::ScramError::Malformed) => Ok(Err(protocol("malformed SCRAM message"))),
    }
}

/// Runs a Query's statements, writing their results; the first error is
/// returned (not yet written).
/// A parameter: a value in the protocol's encoding (tags 0–10 as the row
/// encoding's version 1, 11 a registered type's name and binary form,
/// 12–19 version 3's).
fn parameter(r: &mut Reader) -> Result<Value, Error> {
    let bad = |_| protocol("malformed parameter");
    let net = |r: &mut Reader| -> Result<nucleus_sql::ext::Net, Error> {
        let family = r.u8().map_err(bad)?;
        let bits = r.u8().map_err(bad)?;
        let n = match family {
            4 => 4,
            6 => 16,
            _ => return Err(protocol("malformed parameter")),
        };
        let mut b = vec![family, bits];
        b.extend_from_slice(r.take(n).map_err(bad)?);
        nucleus_sql::ext::Net::from_bytes(&b).ok_or_else(|| protocol("malformed parameter"))
    };
    match r.peek() {
        Some(12) => {
            r.u8().map_err(bad)?;
            return Ok(Value::Int(i64::from(r.u16().map_err(bad)? as i16)));
        }
        Some(13) => {
            r.u8().map_err(bad)?;
            return Ok(Value::Int(i64::from(r.u32().map_err(bad)? as i32)));
        }
        Some(14) => {
            r.u8().map_err(bad)?;
            return Ok(Value::Real(f32::from_bits(r.u32().map_err(bad)?)));
        }
        Some(15) => {
            r.u8().map_err(bad)?;
            let n = r.u32().map_err(bad)? as usize;
            return Ok(Value::Bytea(r.take(n).map_err(bad)?.into()));
        }
        Some(16) => {
            r.u8().map_err(bad)?;
            let t = r.u64().map_err(bad)? as i64;
            if !(0..=86_400_000_000).contains(&t) {
                return Err(protocol("malformed parameter: a time of day out of range"));
            }
            return Ok(Value::Time(t));
        }
        Some(18) => {
            r.u8().map_err(bad)?;
            return Ok(Value::Inet(net(r)?));
        }
        Some(19) => {
            r.u8().map_err(bad)?;
            let n = net(r)?;
            if n.network() != n {
                return Err(Error::invalid_text(
                    "invalid cidr value: it has bits set to the right of its mask",
                ));
            }
            return Ok(Value::Cidr(n));
        }
        Some(10) => {
            r.u8().map_err(bad)?;
            let n = r.u32().map_err(bad)? as usize;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                items.push(parameter(r)?);
            }
            return Ok(Value::Array(items));
        }
        _ => {}
    }
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
    version: u16,
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
            // (An enum's label is converted by the statement, through
            // the catalog.)
            if let Some(t) = t
                && !v.is_null()
                && !matches!(t, DataType::Enum(_))
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
                m = m.str(&c.name).ty(Some(c.ty), version);
            }
            out.write_all(&m.finish())?;
            for row in &rows {
                let mut m = Msg::new(b'D');
                for (v, c) in row.iter().zip(&columns) {
                    m = if text && !v.is_null() {
                        m.value(&Value::Text(v.render()), None, version)
                    } else {
                        m.value(v, Some(c.ty), version)
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
