//! The nucleus protocol (`ncp`, DOCS §10): one connection.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::ops::ControlFlow;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_sql::encoding::{Reader, decode_value, encode_value};
use nucleus_sql::ext::TypeRef;

/// The value and type tag of a registered type (version 2).
const EXT: u8 = 11;
use nucleus_sql::{DataType, Error, OutColumn, Row, Session, Sink, TxnStatus, Value};

use crate::Config;

/// The newest protocol version this server speaks.  Version 2 adds
/// registered types (value and type tag 11: name and binary form); a
/// version-1 client gets them as text.  Version 3 adds tags 12–20
/// (`smallint`, `integer`, `real`, `bytea`, `time`, `char(n)`, `inet`,
/// `cidr`, enums); an older client gets them as before — integers as
/// `bigint`, a real as a double, the rest as text.  Version 4 logs in by
/// SCRAM-SHA-256 (`A` / `a` messages), so a password never crosses the
/// wire; older clients still send theirs in clear.  Version 5 adds a hint
/// to `E` (what to do about the error; empty: none).  Version 6 streams:
/// `Q` carries a fetch size, and a result fetched in batches pauses with
/// `S` after each until the client asks for more (`F`) or no more (`c`).
/// Version 7 adds the columnar result format (2): batches `B` of a buffer
/// per column instead of a `D` per row; and `COPY … FROM STDIN`: `I`, then
/// the client's data (`d` bytes, `B` batches of typed rows) to `e` (or
/// `f`).
pub const VERSION: u16 = 7;
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
pub(crate) fn wire_value(buf: &mut Vec<u8>, v: &Value, ty: Option<DataType>, version: u16) {
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
        hint: None,
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
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String, Error> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| protocol("string is not UTF-8"))
    }
}

/// `E` for a session of `version` (0: before one was agreed).
fn error_msg(e: &Error, version: u16) -> Vec<u8> {
    let m = Msg::new(b'E').str(e.code).str(&e.message);
    if version >= 5 {
        m.str(e.hint.as_deref().unwrap_or("")).finish()
    } else {
        m.finish()
    }
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
pub fn handle(mut stream: TcpStream, kv: Arc<dyn Kv>, config: &Config, id: u32) -> io::Result<()> {
    let log = |what: &str, text: &str| {
        if config.log_queries {
            eprintln!("nucleus-server: [{id}] {what}: {}", text.trim());
        }
    };
    // Behind a trusted proxy, the address its header names.
    let peer = crate::transport::client_address(&mut stream, &config.trusted_proxies)?;
    // A client that starts with a TLS handshake asks for TLS (§10).
    let mut first = [0u8; 1];
    if stream.peek(&mut first)? == 0 {
        return Ok(());
    }
    let mut tls_info = None;
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
        tls_info = Some(secured.info);
        secured.stream
    } else {
        Box::new(stream)
    };
    let (r, w) = crate::transport::split(stream);
    let mut input = BufReader::new(r);
    let mut out = BufWriter::new(w);

    // Hello, and the login it starts.
    let hello = login(
        &mut input,
        &mut out,
        &kv,
        config,
        tls_info.as_ref(),
        peer,
        &log,
    )?;
    let (version, mut session) = match hello {
        Ok(v) => v,
        Err(e) => {
            out.write_all(&error_msg(&e, 0))?;
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
                let parsed = (|| -> Result<(String, u8, u32, Vec<Value>), Error> {
                    let mut b = Body { b: &body };
                    let sql = b.str()?;
                    let format = b.u8()?;
                    let fetch = if version >= 6 { b.u32()? } else { 0 };
                    let n = b.u16()? as usize;
                    let mut r = Reader::new(b.b);
                    let mut params = Vec::with_capacity(n);
                    for _ in 0..n {
                        params.push(parameter(&mut r)?);
                    }
                    Ok((sql, format, fetch, params))
                })();
                match parsed {
                    Ok((sql, format, fetch, params)) => {
                        log("query", &sql);
                        let query = Query {
                            sql: &sql,
                            format: match format {
                                1 => Format::Text,
                                2 if version >= 7 => Format::Columnar,
                                _ => Format::Typed,
                            },
                            fetch,
                            params: &params,
                        };
                        match run(&mut session, &query, version, &mut input, &mut out)? {
                            Ran::Done => {}
                            Ran::Failed(e) => {
                                log("error", &format!("{} {}", e.code, e.message));
                                out.write_all(&error_msg(&e, version))?;
                            }
                            Ran::Close(e) => {
                                if let Some(e) = e {
                                    out.write_all(&error_msg(&e, version))?;
                                }
                                return out.flush();
                            }
                        }
                    }
                    Err(e) => out.write_all(&error_msg(&e, version))?,
                }
                out.write_all(&ready_msg(session.status()))?;
                out.flush()?;
            }
            b'X' => return Ok(()),
            other => {
                out.write_all(&error_msg(
                    &protocol(format!("unexpected message {:?}", other as char)),
                    version,
                ))?;
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
    tls: Option<&crate::TlsInfo>,
    peer: Option<std::net::IpAddr>,
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
    if let Some(why) = config.throttle.locked(&**kv, &user) {
        return Ok(Err(Error {
            code: "28000",
            message: why,
            hint: None,
        }));
    }
    let context = |method: &str| nucleus_sql::access::Context {
        client_addr: peer,
        front_end: "ncp".into(),
        auth_method: method.into(),
        tls: tls.is_some(),
    };
    let refused = |method: &str| {
        config.throttle.failed(&**kv, &user, peer);
        let message = format!("password authentication failed for user \"{user}\"");
        let _ = nucleus_sql::session::audit_refused_login(
            kv,
            &user,
            context(method),
            "28P01",
            &message,
        );
        Error {
            code: "28P01",
            message,
            hint: None,
        }
    };
    let mut identity = None;
    let mut method = "trust";
    if crate::certificate_login(config, &**kv, tls, &user) {
        log("login", &format!("{user} by its client certificate"));
        method = "certificate";
    } else if config.auth.wants_password() {
        method = crate::password_method(config, &**kv, &user);
        if version >= 4 {
            match exchange(input, out, &**kv, config, &user, tls)? {
                Ok(Proved::Password) => {}
                Ok(Proved::Nothing) => return Ok(Err(refused(method))),
                Ok(Proved::Bearer(id)) => {
                    log("login", &format!("{} by a bearer token", id.user));
                    identity = Some(id);
                    method = "oidc";
                }
                Err(e) => {
                    let _ = nucleus_sql::session::audit_refused_login(
                        kv,
                        &user,
                        context(method),
                        e.code,
                        &e.message,
                    );
                    return Ok(Err(e));
                }
            }
        } else if crate::directory_user(config, &**kv, &user) {
            // A password sent in Hello: to the directory only over TLS.
            if tls.is_none() || !crate::directory_accepts(config, &user, &password).unwrap_or(false)
            {
                return Ok(Err(refused(method)));
            }
        } else if !config.accepts(&**kv, &user, &password) {
            return Ok(Err(refused(method)));
        }
    }
    let mut session = Session::with_registry(kv.clone(), config.registry.clone());
    let logged_in = match &identity {
        Some(id) => session.login_as(id),
        None => session.login(&user),
    }
    .and_then(|()| crate::room_for(config, kv, &session))
    .and_then(|()| session.admit(context(method)));
    if logged_in.is_ok() {
        config.throttle.succeeded(&user);
    }
    Ok(match logged_in {
        Ok(()) => Ok((version, session)),
        Err(e) => Err(e),
    })
}

/// What a client proved in the exchange.
enum Proved {
    /// Nothing: refused.
    Nothing,
    /// Its password.
    Password,
    /// Who a bearer token says it is.
    Bearer(nucleus_sql::access::Identity),
}

/// An OAUTHBEARER initial response (RFC 7628): a GS2 header, then
/// `\x01`-separated `key=value` pairs, `auth=Bearer <token>` among them.
fn bearer_token(message: &str) -> Option<&str> {
    let (_gs2, pairs) = message.split_once('\x01')?;
    pairs
        .split('\x01')
        .find_map(|kv| kv.strip_prefix("auth="))
        .and_then(|a| {
            a.strip_prefix("Bearer ")
                .or_else(|| a.strip_prefix("bearer "))
        })
        .map(str::trim)
}

/// The authentication exchange: `A` names the mechanisms, then the
/// client's `a` takes one.  SCRAM-SHA-256: `a` (client first) / `A`
/// (server first) / `a` (client final) / `A` (server final); over TLS with a
/// channel binding SCRAM-SHA-256-PLUS is offered too, and the client's GS2
/// header says which it takes.  OAUTHBEARER (when the server has a
/// [`crate::Bearer`]): one `a` with the token (RFC 7628's initial
/// response), which names the user — Hello's is not used.  PLAIN (RFC
/// 4616), instead of SCRAM, for a role whose password a [`crate::Directory`]
/// checks — over TLS only: one `a` with the password.
fn exchange(
    input: &mut impl Read,
    out: &mut impl Write,
    kv: &dyn Kv,
    config: &Config,
    user: &str,
    tls: Option<&crate::TlsInfo>,
) -> io::Result<Result<Proved, Error>> {
    let mut server = crate::scram_server(config, kv, user);
    let directory = crate::directory_user(config, kv, user);
    let mut mechanisms = if directory {
        if tls.is_none() {
            return Ok(Err(Error {
                code: "28000",
                message: format!(
                    "the password of \"{user}\" is checked by a directory, which needs TLS"
                ),
                hint: None,
            }));
        }
        String::from("PLAIN")
    } else {
        String::from("SCRAM-SHA-256")
    };
    if let Some(b) = tls.and_then(|t| t.channel_binding.as_deref())
        && !directory
    {
        server = server.with_channel_binding(b.to_vec());
        mechanisms = "SCRAM-SHA-256-PLUS SCRAM-SHA-256".into();
    }
    if config.bearer.is_some() {
        mechanisms.push_str(" OAUTHBEARER");
    }
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
    send(out, &mechanisms)?;
    let first = match receive(input)? {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    if directory && first.contains('\0') {
        // PLAIN: [authzid] NUL authcid NUL password; the user is Hello's.
        let mut parts = first.splitn(3, '\0');
        let (_authzid, authcid, password) = (parts.next(), parts.next(), parts.next());
        if authcid.is_some_and(|a| !a.is_empty() && a != user) {
            return Ok(Ok(Proved::Nothing));
        }
        return Ok(
            match crate::directory_accepts(config, user, password.unwrap_or_default()) {
                Ok(true) => Ok(Proved::Password),
                Ok(false) => Ok(Proved::Nothing),
                Err(why) => Err(Error {
                    code: "08006",
                    message: format!("the directory could not check the password: {why}"),
                    hint: None,
                }),
            },
        );
    }
    if directory {
        return Ok(Err(protocol("expected a PLAIN response")));
    }
    if let Some(bearer) = &config.bearer
        && first.contains('\x01')
    {
        let Some(token) = bearer_token(&first) else {
            return Ok(Err(protocol("malformed OAUTHBEARER message")));
        };
        return Ok(match bearer.verify(kv, token) {
            Ok(id) => Ok(Proved::Bearer(id)),
            Err(why) => Err(Error {
                code: "28000",
                message: format!("the bearer token was not accepted: {why}"),
                hint: None,
            }),
        });
    }
    let server_first = match server.first(&first) {
        Ok(m) => m,
        // A client that could bind, told it cannot: someone in between.
        Err(nucleus_crypto::ScramError::Rejected) => return Ok(Ok(Proved::Nothing)),
        Err(nucleus_crypto::ScramError::Malformed) => {
            return Ok(Err(protocol("malformed SCRAM message")));
        }
    };
    send(out, &server_first)?;
    let last = match receive(input)? {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    match server.finish(&last) {
        Ok(server_final) => {
            send(out, &server_final)?;
            Ok(Ok(Proved::Password))
        }
        Err(nucleus_crypto::ScramError::Rejected) => Ok(Ok(Proved::Nothing)),
        Err(nucleus_crypto::ScramError::Malformed) => Ok(Err(protocol("malformed SCRAM message"))),
    }
}

/// A parameter: a value in the protocol's encoding (tags 0–10 as the row
/// encoding's version 1, 11 a registered type's name and binary form,
/// 12–19 version 3's).
pub(crate) fn parameter(r: &mut Reader) -> Result<Value, Error> {
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
        hint: None,
    })?;
    // The binary form is the canonical one; checked, as any input is.
    let v = Value::ext(t, bytes.to_vec());
    t.imp()
        .input(&t.imp().output(&v))
        .map_err(|_| protocol("malformed parameter"))?;
    Ok(v)
}

/// How a result's rows go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// A `D` per row, typed values.
    Typed,
    /// A `D` per row, every value as text.
    Text,
    /// Batches `B`, a buffer per column (version 7).
    Columnar,
}

/// A `Q` message.
struct Query<'a> {
    sql: &'a str,
    format: Format,
    /// Rows per batch (0: the whole result at once).
    fetch: u32,
    params: &'a [Value],
}

/// How a `Q` went.
enum Ran {
    /// Every statement ran.
    Done,
    /// A statement failed: the error to send.
    Failed(Error),
    /// The client left while a result waited (`X`), or broke the protocol
    /// (the error to send): the connection ends.
    Close(Option<Error>),
}

/// A statement's rows onto the wire as the executor produces them.  With a
/// fetch size, after each batch an `S` and a wait for the client: `F`
/// (the next batch; its size, 0 for the rest) or `c` (no more — the
/// statement ends there).
struct Wire<'a, R: Read, W: Write> {
    input: &'a mut R,
    out: &'a mut W,
    format: Format,
    version: u16,
    types: Vec<DataType>,
    fetch: u32,
    /// Rows left in this batch (`None`: no limit).
    left: Option<u32>,
    /// Columnar: the batch being filled, and its size.
    pending: Option<crate::columnar::Builder>,
    batch: usize,
    /// `COPY … FROM STDIN`: the columns, while the client's data comes.
    copying: Option<Vec<DataType>>,
    /// The connection failed: the statement stopped on it.
    broken: Option<io::Error>,
    /// How the connection ends instead of going on.
    close: Option<Option<Error>>,
}

impl<R: Read, W: Write> Wire<'_, R, W> {
    fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.out.write_all(bytes).map_err(|e| self.broke(e))
    }

    fn broke(&mut self, e: io::Error) -> Error {
        self.broken = Some(e);
        Error {
            code: "08006",
            message: "the connection failed while the result was sent".into(),
            hint: None,
        }
    }

    /// Columnar: sends the rows gathered as a batch.
    fn send_batch(&mut self) -> Result<(), Error> {
        let Some(b) = self.pending.as_mut().filter(|b| !b.is_empty()) else {
            return Ok(());
        };
        let b = b.finish();
        self.send(&b)
    }

    /// Columnar: room for a row — the full batch sent first, and with a
    /// fetch size a wait for the client; `Break` when it wants no more.
    fn room(&mut self) -> Result<ControlFlow<()>, Error> {
        if self.pending.as_ref().is_some_and(|b| b.len() >= self.batch) {
            self.send_batch()?;
            if self.left.is_some() {
                match self.suspend()? {
                    Some(next) => {
                        self.left = next;
                        self.batch = next.map_or(crate::columnar::DEFAULT_BATCH, |n| n as usize);
                    }
                    None => return Ok(ControlFlow::Break(())),
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Pauses after a batch: the next one's size (`None`: no limit), or
    /// none when the client wants no more.
    fn suspend(&mut self) -> Result<Option<Option<u32>>, Error> {
        self.send(&Msg::new(b'S').finish())?;
        self.out.flush().map_err(|e| self.broke(e))?;
        let input = &mut *self.input;
        match nucleus_sql::budget::paused(|| read_message(input)) {
            Ok((b'F', body)) => {
                let n = Body { b: &body }.u32()?;
                Ok(Some((n > 0).then_some(n)))
            }
            Ok((b'c', _)) => Ok(None),
            Ok((b'X', _)) => {
                self.close = Some(None);
                Err(protocol("the client left"))
            }
            Ok((other, _)) => {
                let e = protocol(format!(
                    "unexpected message {:?} while a result waits for F or c",
                    other as char
                ));
                self.close = Some(Some(e.clone()));
                Err(e)
            }
            Err(e) => Err(self.broke(e)),
        }
    }
}

impl<R: Read, W: Write> Wire<'_, R, W> {
    /// After a `COPY` that ended early (an error): the client's remaining
    /// data, read and dropped up to its end.
    fn skip_copy(&mut self) -> io::Result<()> {
        while self.copying.is_some() {
            match read_message(self.input)? {
                (b'e' | b'f', _) => self.copying = None,
                (b'X', _) => {
                    self.copying = None;
                    self.close = Some(None);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl<R: Read, W: Write> Sink for Wire<'_, R, W> {
    fn copy_in(&mut self, columns: &[OutColumn]) -> Result<(), Error> {
        if self.version < 7 {
            return Err(Error::feature(
                "COPY FROM STDIN needs protocol version 7 (a newer driver)",
            ));
        }
        let mut m = Msg::new(b'I').u16(columns.len() as u16);
        for c in columns {
            m = m.str(&c.name).ty(Some(c.ty), self.version);
        }
        self.copying = Some(columns.iter().map(|c| c.ty).collect());
        self.send(&m.finish())?;
        self.out.flush().map_err(|e| self.broke(e))
    }

    fn copy_data(&mut self) -> Result<Option<nucleus_sql::copy::Chunk>, Error> {
        use nucleus_sql::copy::Chunk;
        let input = &mut *self.input;
        match nucleus_sql::budget::paused(|| read_message(input)) {
            Ok((b'd', body)) => Ok(Some(Chunk::Bytes(body))),
            Ok((b'B', body)) => {
                let types = self.copying.clone().unwrap_or_default();
                crate::columnar::rows(&body, &types).map(|r| Some(Chunk::Rows(r)))
            }
            Ok((b'e', _)) => {
                self.copying = None;
                Ok(None)
            }
            Ok((b'f', body)) => {
                self.copying = None;
                let why = Body { b: &body }.str().unwrap_or_default();
                Err(Error::query_canceled(format!(
                    "COPY from stdin failed: {why}"
                )))
            }
            Ok((b'X', _)) => {
                self.copying = None;
                self.close = Some(None);
                Err(protocol("the client left"))
            }
            Ok((other, _)) => {
                self.copying = None;
                let e = protocol(format!(
                    "unexpected message {:?} during COPY data",
                    other as char
                ));
                self.close = Some(Some(e.clone()));
                Err(e)
            }
            Err(e) => Err(self.broke(e)),
        }
    }

    fn columns(&mut self, columns: &[OutColumn]) -> Result<(), Error> {
        let mut m = Msg::new(b'C').u16(columns.len() as u16);
        for c in columns {
            m = m.str(&c.name).ty(Some(c.ty), self.version);
        }
        self.types = columns.iter().map(|c| c.ty).collect();
        if self.format == Format::Columnar {
            self.pending = Some(crate::columnar::Builder::new(&self.types, self.version));
        }
        self.left = (self.fetch > 0).then_some(self.fetch);
        self.batch = match self.fetch {
            0 => crate::columnar::DEFAULT_BATCH,
            n => n as usize,
        };
        self.send(&m.finish())
    }

    fn row(&mut self, row: Row) -> Result<ControlFlow<()>, Error> {
        if let Some(flow) = self.columnar(|b| b.push_row(&row).map(|()| 1))? {
            return Ok(match flow {
                ControlFlow::Continue(_) => ControlFlow::Continue(()),
                ControlFlow::Break(()) => ControlFlow::Break(()),
            });
        }
        if self.left == Some(0) {
            match self.suspend()? {
                Some(next) => self.left = next,
                None => return Ok(ControlFlow::Break(())),
            }
        }
        if let Some(n) = &mut self.left {
            *n -= 1;
        }
        let mut m = Msg::new(b'D');
        for (v, t) in row.iter().zip(&self.types) {
            m = if self.format == Format::Text && !v.is_null() {
                m.value(&Value::Text(v.render()), None, self.version)
            } else {
                m.value(v, Some(*t), self.version)
            };
        }
        self.send(&m.finish())?;
        Ok(ControlFlow::Continue(()))
    }

    fn takes_batches(&self) -> bool {
        self.format == Format::Columnar
    }

    fn batch(&mut self, b: &nucleus_sql::vector::Batch) -> Result<ControlFlow<usize>, Error> {
        if self.format != Format::Columnar {
            for (k, i) in b.sel.iter().enumerate() {
                if self.row(b.row(i))?.is_break() {
                    return Ok(ControlFlow::Break(k));
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        // As much as each batch on the wire has room for.
        let mut k = 0;
        while k < b.sel.len() {
            let batch = self.batch;
            let flow = self.columnar(|p| {
                let n = (batch.saturating_sub(p.len())).clamp(1, b.sel.len() - k);
                p.push_batch(b, k, k + n).map(|()| n)
            })?;
            match flow {
                Some(ControlFlow::Continue(n)) => k += n,
                _ => return Ok(ControlFlow::Break(k)),
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

impl<R: Read, W: Write> Wire<'_, R, W> {
    /// Columnar: `push` into the batch being built once it has room (its
    /// rows taken, or `Break` when the client wants no more); `None` for
    /// another format.
    fn columnar(
        &mut self,
        push: impl FnOnce(&mut crate::columnar::Builder) -> Result<usize, Error>,
    ) -> Result<Option<ControlFlow<(), usize>>, Error> {
        if self.format != Format::Columnar {
            return Ok(None);
        }
        if self.room()?.is_break() {
            return Ok(Some(ControlFlow::Break(())));
        }
        let b = self.pending.as_mut().expect("columns before rows");
        Ok(Some(ControlFlow::Continue(push(b)?)))
    }
}

/// Runs a Query's statements, writing their results as they are produced;
/// the first error is returned (not yet written).
fn run<R: Read, W: Write>(
    session: &mut Session,
    query: &Query,
    version: u16,
    input: &mut R,
    out: &mut W,
) -> io::Result<Ran> {
    let (stmts, texts): (Vec<_>, Vec<_>) = match nucleus_sql::parser::parse_with_text(query.sql) {
        Ok(s) => s.into_iter().unzip(),
        Err(e) => return Ok(Ran::Failed(e)),
    };
    if !query.params.is_empty() && stmts.len() > 1 {
        return Ok(Ran::Failed(Error::syntax(
            "a query with parameters must hold one statement",
        )));
    }
    if query.fetch > 0 && stmts.len() > 1 {
        return Ok(Ran::Failed(Error::syntax(
            "a query fetched in batches must hold one statement",
        )));
    }
    // Parameters take the types their places need (text is parsed).
    let mut converted = query.params.to_vec();
    if let [stmt] = stmts.as_slice()
        && !query.params.is_empty()
    {
        let types = match session.describe(stmt) {
            Ok((types, _)) => types,
            Err(e) => return Ok(Ran::Failed(e)),
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
                    Err(e) => return Ok(Ran::Failed(e)),
                }
            }
        }
    }
    let params = converted.as_slice();
    for (stmt, stmt_text) in stmts.iter().zip(&texts) {
        session.note_text(stmt_text);
        let mut wire = Wire {
            input: &mut *input,
            out: &mut *out,
            format: query.format,
            version,
            types: Vec::new(),
            fetch: query.fetch,
            left: None,
            pending: None,
            batch: crate::columnar::DEFAULT_BATCH,
            copying: None,
            broken: None,
            close: None,
        };
        let mut r = session.run_into(stmt, params, &mut wire);
        if wire.broken.is_none() {
            wire.skip_copy()?;
        }
        // The last batch, before the end (or the error that ended it).
        if let Err(e) = wire.send_batch()
            && r.is_ok()
        {
            r = Err(e);
        }
        if let Some(e) = wire.broken {
            return Err(e);
        }
        if let Some(close) = wire.close {
            return Ok(Ran::Close(close));
        }
        let tag = match r {
            Ok(tag) => tag,
            Err(e) => return Ok(Ran::Failed(e)),
        };
        let (command, count) = split_tag(&tag);
        out.write_all(&Msg::new(b'K').str(&command).u64(count).finish())?;
    }
    Ok(Ran::Done)
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
