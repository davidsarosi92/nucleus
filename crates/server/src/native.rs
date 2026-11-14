//! The nucleus protocol (`ncp`, DOCS §10): one connection.

use std::collections::VecDeque;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::ops::ControlFlow;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_sql::ext::TypeRef;
use nucleus_sql::{DataType, Error, OutColumn, Row, Session, Sink, TxnStatus, Value};
use nucleus_wire::{Reader, WireValue, message_waiting, put, read_value, tag};

use crate::Config;

/// The connection's read and write buffers: 64 KiB, so a large result
/// goes out in few writes (and, over TLS, in full-size records) rather
/// than one per 8 KiB.
const WIRE_BUFFER: usize = 64 << 10;

/// The newest protocol version this server speaks ([`nucleus_wire::VERSION`]).  Version 2 adds
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
pub const VERSION: u16 = nucleus_wire::VERSION;
/// The oldest it still speaks.
pub const MIN_VERSION: u16 = nucleus_wire::MIN_VERSION;

/// An outgoing message.
struct Msg {
    buf: Vec<u8>,
}

impl Msg {
    fn new(kind: u8) -> Self {
        let mut buf = Vec::new();
        nucleus_wire::begin_frame(&mut buf, kind);
        Self { buf }
    }
    fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }
    fn u16(mut self, v: u16) -> Self {
        put::u16(&mut self.buf, v);
        self
    }
    fn u32(mut self, v: u32) -> Self {
        put::u32(&mut self.buf, v);
        self
    }
    fn u64(mut self, v: u64) -> Self {
        put::u64(&mut self.buf, v);
        self
    }
    fn str(mut self, s: &str) -> Self {
        put::str(&mut self.buf, s);
        self
    }
    fn ty(mut self, t: Option<DataType>, version: u16) -> Self {
        match t {
            Some(DataType::Ext(t)) if version >= 2 => self.u8(tag::EXT).str(t.name()),
            Some(DataType::Enum(e)) if version >= 3 => self.u8(tag::ENUM).str(e.name()),
            None => self.u8(tag::NULL),
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
        nucleus_wire::end_frame(&mut self.buf, 0);
        self.buf
    }
}

/// A column type's tag in protocol `version` (DOCS §10).
pub(crate) fn type_tag(t: DataType, version: u16) -> u8 {
    let v3 = version >= 3;
    match t {
        DataType::Bool => tag::BOOL,
        DataType::SmallInt if v3 => tag::SMALLINT,
        DataType::Integer if v3 => tag::INTEGER,
        DataType::SmallInt | DataType::Integer | DataType::BigInt => tag::BIGINT,
        DataType::Real if v3 => tag::REAL,
        DataType::Real | DataType::Float => tag::DOUBLE,
        DataType::Bytea if v3 => tag::BYTEA,
        DataType::Time if v3 => tag::TIME,
        DataType::Char if v3 => tag::CHAR,
        DataType::Inet if v3 => tag::INET,
        DataType::Cidr if v3 => tag::CIDR,
        DataType::Text
        | DataType::Char
        | DataType::Bytea
        | DataType::Time
        | DataType::Inet
        | DataType::Cidr => tag::TEXT,
        DataType::Numeric => tag::NUMERIC,
        DataType::Date => tag::DATE,
        DataType::Timestamp => tag::TIMESTAMP,
        DataType::TimestampTz => tag::TIMESTAMPTZ,
        DataType::Interval => tag::INTERVAL,
        DataType::Array(_) => tag::ARRAY,
        // Registered types (to version-1 clients) and enums (before
        // version 3) travel as text.
        DataType::Ext(_) | DataType::Enum(_) => tag::TEXT,
    }
}

/// Appends a value of column type `ty` in the protocol's encoding (DOCS
/// §10, [`nucleus_wire`]) — the protocol's own, not the storage's.
pub(crate) fn wire_value(buf: &mut Vec<u8>, v: &Value, ty: Option<DataType>, version: u16) {
    let v3 = version >= 3;
    match v {
        Value::Null => put::null(buf),
        Value::Bool(b) => put::bool(buf, *b),
        Value::Int(i) => match ty {
            Some(DataType::SmallInt) if v3 => put::smallint(buf, *i as i16),
            Some(DataType::Integer) if v3 => put::integer(buf, *i as i32),
            _ => put::bigint(buf, *i),
        },
        Value::Real(f) if v3 => put::real(buf, *f),
        Value::Real(f) => put::double(buf, f64::from(*f)),
        Value::Float(f) => put::double(buf, *f),
        Value::Text(s) => put::text(buf, s),
        Value::Numeric(d) => put::numeric(buf, d.mantissa(), d.scale() as u8),
        Value::Date(d) => put::date(buf, *d),
        Value::Timestamp(t) => put::timestamp(buf, *t, false),
        Value::TimestampTz(t) => put::timestamp(buf, *t, true),
        Value::Interval(i) => put::interval(buf, i.months, i.days, i.micros),
        Value::Bytea(b) if v3 => put::bytea(buf, b),
        Value::Time(t) if v3 => put::time(buf, *t),
        Value::Inet(n) | Value::Cidr(n) if v3 => {
            let b = n.to_bytes();
            put::net(buf, matches!(v, Value::Cidr(_)), b[0], b[1], &b[2..]);
        }
        // char(n) and enum values are text (their column types say what
        // they are); older clients get the others as text too.
        Value::Bytea(_)
        | Value::Time(_)
        | Value::Char(_)
        | Value::Inet(_)
        | Value::Cidr(_)
        | Value::Enum(_) => put::text(buf, &v.render()),
        Value::Array(items) => {
            put::array(buf, items.len());
            let elem = ty.and_then(DataType::element);
            for item in items {
                wire_value(buf, item, elem, version);
            }
        }
        Value::Ext(t, bytes) if version >= 2 => match t.imp().wire_output(v) {
            Some(form) => put::ext(buf, t.name(), &form),
            None => put::ext(buf, t.name(), bytes),
        },
        // Registered types travel as text to version-1 clients.
        Value::Ext(..) => put::text(buf, &v.render()),
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
    nucleus_wire::read_frame(r)
}

/// Reads the fields of a client message.
struct Body<'a> {
    r: Reader<'a>,
}

impl<'a> Body<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { r: Reader::new(b) }
    }
    fn u8(&mut self) -> Result<u8, Error> {
        self.r.u8().map_err(malformed)
    }
    fn u16(&mut self) -> Result<u16, Error> {
        self.r.u16().map_err(malformed)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        self.r.u32().map_err(malformed)
    }
    fn str(&mut self) -> Result<String, Error> {
        self.r.str().map_err(malformed)
    }
}

fn malformed(e: nucleus_wire::Malformed) -> Error {
    protocol(e.0)
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

fn logger(config: &Config, id: u32) -> impl Fn(&str, &str) + '_ {
    move |what: &str, text: &str| {
        if config.log_queries {
            eprintln!(
                "nucleus-server: [{id}] {what}: {}",
                nucleus_sql::statements::redacted(text.trim())
            );
        }
    }
}

/// Serves one client until it says Bye or goes away.
pub fn handle(mut stream: TcpStream, kv: Arc<dyn Kv>, config: &Config, id: u32) -> io::Result<()> {
    let log = logger(config, id);
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
    serve(stream, kv, config, id, peer, tls_info)
}

/// Serves one client over any byte stream until it says Bye or goes
/// away — a connection made in the process, or a fuzzer's bytes: no TLS
/// and no client address.
pub fn handle_stream(
    stream: Box<dyn crate::Stream>,
    kv: Arc<dyn Kv>,
    config: &Config,
    id: u32,
) -> io::Result<()> {
    serve(stream, kv, config, id, None, None)
}

fn serve(
    stream: Box<dyn crate::Stream>,
    kv: Arc<dyn Kv>,
    config: &Config,
    id: u32,
    peer: Option<std::net::IpAddr>,
    tls_info: Option<crate::TlsInfo>,
) -> io::Result<()> {
    let log = logger(config, id);
    let (r, w) = crate::transport::split(stream);
    let mut input = BufReader::with_capacity(WIRE_BUFFER, r);
    let mut out = BufWriter::with_capacity(WIRE_BUFFER, w);

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
    welcome(&mut out, version, id, &session)?;
    out.flush()?;

    loop {
        let (kind, body) = read_message(&mut input)?;
        match message(
            &mut session,
            version,
            kind,
            &body,
            &mut input,
            &mut out,
            &log,
            false,
        )? {
            Next::Go => {
                // A client that sent the next message already (pipelined)
                // gets the replies together: written when it waits.
                if !message_waiting(input.buffer()) {
                    out.flush()?;
                }
            }
            Next::End => return out.flush(),
        }
    }
}

/// Welcome (the version agreed, the server, the session's id) and the
/// first Ready.
fn welcome(out: &mut impl Write, version: u16, id: u32, session: &Session) -> io::Result<()> {
    out.write_all(
        &Msg::new(b'W')
            .u16(version)
            .str(&format!("nucleus {}", env!("CARGO_PKG_VERSION")))
            .u32(id)
            .finish(),
    )?;
    out.write_all(&ready_msg(session.status()))
}

/// What the session does after a message.
enum Next {
    /// Reads the next.
    Go,
    /// Ends (Bye, or an error that closes it).
    End,
}

/// Answers one client message (after the login); `input` holds what a
/// statement reads beyond it (`COPY` data, a stream's fetches); `stepped`:
/// for an [`Exchange`], where nothing more can come while it runs.
// The session's parts, each its own (a struct of them would be built for
// this one call).
#[allow(clippy::too_many_arguments)]
fn message<R: Read, W: Write>(
    session: &mut Session,
    version: u16,
    kind: u8,
    body: &[u8],
    input: &mut BufReader<R>,
    out: &mut W,
    log: &dyn Fn(&str, &str),
    stepped: bool,
) -> io::Result<Next> {
    match kind {
        b'Q' => {
            let parsed = (|| -> Result<(String, u8, u32, Vec<Value>), Error> {
                let mut b = Body::new(body);
                let sql = b.str()?;
                let format = b.u8()?;
                let fetch = if version >= 6 { b.u32()? } else { 0 };
                let n = b.u16()? as usize;
                let mut r = b.r;
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
                        stepped,
                    };
                    match run(session, &query, version, input, out)? {
                        Ran::Done => {}
                        Ran::Failed(e) => {
                            log("error", &format!("{} {}", e.code, e.message));
                            out.write_all(&error_msg(&e, version))?;
                        }
                        Ran::Close(e) => {
                            if let Some(e) = e {
                                out.write_all(&error_msg(&e, version))?;
                            }
                            return Ok(Next::End);
                        }
                    }
                }
                Err(e) => out.write_all(&error_msg(&e, version))?,
            }
            out.write_all(&ready_msg(session.status()))?;
            Ok(Next::Go)
        }
        b'X' => Ok(Next::End),
        // Copy data after a COPY that failed before it took any: a client
        // that sends it with the statement (the embedded edition in
        // WebAssembly does) has nothing to wait for — dropped, as
        // PostgreSQL drops it.
        b'd' | b'e' | b'f' => Ok(Next::Go),
        other => {
            out.write_all(&error_msg(
                &protocol(format!("unexpected message {:?}", other as char)),
                version,
            ))?;
            Ok(Next::End)
        }
    }
}

/// One session's server driven by the bytes it is given rather than by a
/// thread waiting for them (sans-IO): each [`feed`](Self::feed) runs the
/// whole messages among what it has and returns the replies.  For a
/// platform without threads — the embedded edition in WebAssembly
/// (PLAN.md §M14.23) — where nothing can wait in the middle of a
/// statement, so a client sends what a statement reads with it: `COPY`
/// data with the `COPY` (a statement that would wait for more ends the
/// session, `08P01`), and results fetched whole (`fetch` 0).
pub struct Exchange {
    kv: Arc<dyn Kv>,
    config: Config,
    id: u32,
    /// The protocol version and session, once logged in.
    session: Option<(u16, Session)>,
    /// What came in and is not yet a whole message.
    input: Vec<u8>,
    ended: bool,
}

impl Exchange {
    /// A session not yet greeted (Hello is the first message).
    pub fn new(kv: Arc<dyn Kv>, config: Config, id: u32) -> Self {
        Self {
            kv,
            config,
            id,
            session: None,
            input: Vec::new(),
            ended: false,
        }
    }

    /// Whether the session has ended (Bye, a failed login, a protocol
    /// error); it then takes nothing more.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Takes the client's bytes and runs each whole message among them
    /// (with what follows it, if its statement reads more); returns what
    /// the server sends back.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        self.input.extend_from_slice(bytes);
        while !self.ended && message_waiting(&self.input) {
            let input = std::mem::take(&mut self.input);
            let mut reader = BufReader::new(&input[..]);
            match self.step(&mut reader, &mut out) {
                Ok(Next::Go) => {}
                Ok(Next::End) => self.ended = true,
                Err(e) => {
                    // A statement that read past what was sent (or bytes
                    // that are no message): nothing can wait here.
                    let e = protocol(format!(
                        "a statement read past what was sent ({e}): send COPY data \
                         with its COPY, fetch results whole"
                    ));
                    let version = self.session.as_ref().map_or(0, |(v, _)| *v);
                    out.extend_from_slice(&error_msg(&e, version));
                    self.ended = true;
                }
            }
            let rest = reader.buffer().len() + reader.get_ref().len();
            self.input = input[input.len() - rest..].to_vec();
        }
        out
    }

    fn step(&mut self, input: &mut BufReader<&[u8]>, out: &mut Vec<u8>) -> io::Result<Next> {
        let log = logger(&self.config, self.id);
        let Some((version, session)) = self.session.as_mut() else {
            match login(input, out, &self.kv, &self.config, None, None, &log)? {
                Ok((version, session)) => {
                    welcome(out, version, self.id, &session)?;
                    self.session = Some((version, session));
                    return Ok(Next::Go);
                }
                Err(e) => {
                    out.write_all(&error_msg(&e, 0))?;
                    return Ok(Next::End);
                }
            }
        };
        let (kind, body) = read_message(input)?;
        message(session, *version, kind, &body, input, out, &log, true)
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
        let mut b = Body::new(&body);
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
        Ok(Body::new(&body).str())
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
    let v = read_value(r).map_err(|_| protocol("malformed parameter"))?;
    from_wire(v)
}

/// The value a parameter carried.
fn from_wire(v: WireValue) -> Result<Value, Error> {
    let bad = |_| protocol("malformed parameter");
    Ok(match v {
        WireValue::Null => Value::Null,
        WireValue::Bool(b) => Value::Bool(b),
        WireValue::SmallInt(i) => Value::Int(i64::from(i)),
        WireValue::Integer(i) => Value::Int(i64::from(i)),
        WireValue::BigInt(i) => Value::Int(i),
        WireValue::Real(f) => Value::Real(f),
        WireValue::Double(f) => Value::Float(f),
        WireValue::Text(s) => Value::Text(s),
        WireValue::Numeric { mantissa, scale } => {
            Value::Numeric(nucleus_sql::ext::Decimal::new(mantissa, u32::from(scale)).map_err(bad)?)
        }
        WireValue::Date(d) => Value::Date(d),
        WireValue::Timestamp(t) => Value::Timestamp(t),
        WireValue::TimestampTz(t) => Value::TimestampTz(t),
        WireValue::Interval {
            months,
            days,
            micros,
        } => Value::Interval(nucleus_sql::ext::Interval {
            months,
            days,
            micros,
        }),
        WireValue::Array(items) => {
            Value::Array(items.into_iter().map(from_wire).collect::<Result<_, _>>()?)
        }
        WireValue::Bytea(b) => Value::Bytea(b.into()),
        WireValue::Time(t) => {
            if !(0..=86_400_000_000).contains(&t) {
                return Err(protocol("malformed parameter: a time of day out of range"));
            }
            Value::Time(t)
        }
        WireValue::Net {
            cidr,
            family,
            bits,
            addr,
        } => {
            let mut b = vec![family, bits];
            b.extend_from_slice(&addr[..nucleus_wire::net_len(family)]);
            let n = nucleus_sql::ext::Net::from_bytes(&b)
                .ok_or_else(|| protocol("malformed parameter"))?;
            if !cidr {
                Value::Inet(n)
            } else if n.network() != n {
                return Err(Error::invalid_text(
                    "invalid cidr value: it has bits set to the right of its mask",
                ));
            } else {
                Value::Cidr(n)
            }
        }
        WireValue::Ext { type_name, bytes } => {
            let t = TypeRef::named(&type_name).ok_or_else(|| Error {
                code: "42704",
                message: format!("type \"{type_name}\" does not exist"),
                hint: None,
            })?;
            // Checked as any input is, through the type's text form, and
            // kept as that reads back: the canonical bytes (a sparsevec's
            // positions in order, its zeros dropped).
            let checked = match t.imp().wire_input(&bytes) {
                Some(v) => v,
                None => {
                    let v = Value::ext(t, bytes);
                    t.imp().input(&t.imp().output(&v))
                }
            };
            checked.map_err(|e| Error {
                code: e.code,
                message: format!("malformed parameter: {}", e.message),
                hint: None,
            })?
        }
    })
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
    /// Driven by the bytes given ([`Exchange`]): nothing more comes while
    /// it runs.
    stepped: bool,
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
/// statement ends there).  While the client handles a batch, the next is
/// produced: the rows after an `S` wait here, up to a batch of the size
/// last asked for, and go out when the client asks for them (or are
/// dropped when it wants no more).
struct Wire<'a, R: Read, W: Write> {
    input: &'a mut BufReader<R>,
    out: &'a mut W,
    format: Format,
    version: u16,
    types: Vec<DataType>,
    fetch: u32,
    /// Rows left in the batch the client asked for (`None`: no limit).
    left: Option<u32>,
    /// Columnar: the batch being filled.
    pending: Option<crate::columnar::Builder>,
    /// Rows a batch holds: columnar, one `B`'s; with a fetch size, the
    /// size last asked for (what is read ahead).
    batch: usize,
    /// An `S` sent, the client's answer not read yet.
    waiting: bool,
    /// Rows (`D` messages) produced while `waiting`; columnar ones wait in
    /// `pending`.
    ahead: VecDeque<Vec<u8>>,
    /// Rows sent.
    sent: u64,
    /// The client wanted no more (`c`).
    stopped: bool,
    /// `COPY … FROM STDIN`: the columns, while the client's data comes.
    copying: Option<Vec<DataType>>,
    /// The connection failed: the statement stopped on it.
    broken: Option<io::Error>,
    /// How the connection ends instead of going on.
    close: Option<Option<Error>>,
    /// Driven by the bytes given ([`Exchange`]): a batch of a fetch is
    /// followed by the next without an `S` (no answer can come), a `COPY`
    /// without its data fails, and the reply is bounded
    /// ([`STEPPED_REPLY`]).
    stepped: bool,
    /// The bytes sent (stepped: what the reply holds).
    replied: usize,
}

/// The most a stepped session's reply to one statement holds: it is in
/// memory whole, in a 32-bit address space.
const STEPPED_REPLY: usize = 256 << 20;

impl<R: Read, W: Write> Wire<'_, R, W> {
    fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.replied += bytes.len();
        if self.stepped && self.replied > STEPPED_REPLY {
            return Err(Error {
                code: "54000",
                message: format!(
                    "the result is past {} MB, more than a session without threads \
                     sends at once",
                    STEPPED_REPLY >> 20
                ),
                hint: Some("fetch it in parts (LIMIT and a key to go on from)".into()),
            });
        }
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
        self.sent += b.len() as u64;
        let b = b.finish();
        self.send(&b)
    }

    /// Columnar: room for a row — a full batch sent first, and with a
    /// fetch size an `S` after it; `Break` when the client wants no more.
    fn room(&mut self) -> Result<ControlFlow<()>, Error> {
        loop {
            let len = self.pending.as_ref().map_or(0, |b| b.len());
            if self.waiting && (len >= self.batch || self.answered()) {
                if self.resolve()?.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
                continue;
            }
            if !self.waiting && len >= self.batch {
                self.send_batch()?;
                if self.left.is_some() {
                    self.pause()?;
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
    }

    /// Sends `S` after a batch: what is produced until the client answers
    /// waits.
    fn pause(&mut self) -> Result<(), Error> {
        if self.stepped {
            // No answer can come: the next batch, as if asked for.
            self.left = (self.fetch > 0).then_some(self.fetch);
            return Ok(());
        }
        self.send(&Msg::new(b'S').finish())?;
        self.out.flush().map_err(|e| self.broke(e))?;
        self.waiting = true;
        Ok(())
    }

    /// Whether the client's answer to `S` has come (without waiting for
    /// it).
    fn answered(&self) -> bool {
        self.waiting && message_waiting(self.input.buffer())
    }

    /// The client's answer to `S`: on `F` the rows read ahead go out, as
    /// many as it asked for (another `S` after them when more are left); on
    /// `c` they are dropped and `Break`.
    fn resolve(&mut self) -> Result<ControlFlow<()>, Error> {
        self.waiting = false;
        let input = &mut *self.input;
        let next = match nucleus_sql::budget::paused(|| read_message(input)) {
            Ok((b'F', body)) => {
                let n = Body::new(&body).u32()?;
                (n > 0).then_some(n)
            }
            Ok((b'c', _)) => {
                self.stopped = true;
                self.ahead.clear();
                if let Some(b) = &mut self.pending {
                    b.clear();
                }
                return Ok(ControlFlow::Break(()));
            }
            Ok((b'X', _)) => {
                self.close = Some(None);
                return Err(protocol("the client left"));
            }
            Ok((other, _)) => {
                let e = protocol(format!(
                    "unexpected message {:?} while a result waits for F or c",
                    other as char
                ));
                self.close = Some(Some(e.clone()));
                return Err(e);
            }
            Err(e) => return Err(self.broke(e)),
        };
        self.left = next;
        self.batch = next.map_or(crate::columnar::DEFAULT_BATCH, |n| n as usize);
        if let Some(b) = &mut self.pending {
            // More read ahead than asked for: a batch of the size asked
            // for, the rest kept (rare: drivers ask for the same size).
            if b.len() > self.batch {
                let rows = crate::columnar::rows(&b.finish()[5..], &self.types)?;
                for r in &rows[..self.batch] {
                    b.push_row(r)?;
                }
                self.send_batch()?;
                let b = self.pending.as_mut().expect("columnar");
                for r in &rows[self.batch..] {
                    b.push_row(r)?;
                }
                self.pause()?;
            }
            return Ok(ControlFlow::Continue(()));
        }
        while !self.ahead.is_empty() {
            if self.left == Some(0) {
                self.pause()?;
                break;
            }
            let m = self.ahead.pop_front().expect("a row");
            self.sent_row();
            self.send(&m)?;
        }
        Ok(ControlFlow::Continue(()))
    }

    fn sent_row(&mut self) {
        if let Some(n) = &mut self.left {
            *n -= 1;
        }
        self.sent += 1;
    }

    /// The statement's end (or the error that ended it): the rows read
    /// ahead go out as the client asks for them, then the last batch.
    fn finish(&mut self) -> Result<(), Error> {
        while self.waiting {
            if self.resolve()?.is_break() {
                return Ok(());
            }
        }
        self.send_batch()
    }

    /// After a `COPY` that ended early (an error): the client's remaining
    /// data, read and dropped up to its end.
    fn skip_copy(&mut self) -> io::Result<()> {
        while self.copying.is_some() {
            match read_message(self.input) {
                Ok((b'e' | b'f', _)) => self.copying = None,
                Ok((b'X', _)) => {
                    self.copying = None;
                    self.close = Some(None);
                }
                Ok(_) => {}
                // Stepped: the data sent with the COPY ran out.
                Err(e) if self.stepped && e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.copying = None;
                }
                Err(e) => return Err(e),
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
                let why = Body::new(&body).str().unwrap_or_default();
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
            Err(e) if self.stepped && e.kind() == io::ErrorKind::UnexpectedEof => {
                self.copying = None;
                Err(Error::query_canceled(
                    "COPY from stdin failed: no data came with it (a session \
                     without threads takes a COPY's data sent with the COPY)",
                ))
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
        while self.waiting && (self.ahead.len() >= self.batch || self.answered()) {
            if self.resolve()?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        if !self.waiting && self.left == Some(0) {
            self.pause()?;
        }
        let mut m = Msg::new(b'D');
        for (v, t) in row.iter().zip(&self.types) {
            m = if self.format == Format::Text && !v.is_null() {
                m.value(&Value::Text(v.render()), None, self.version)
            } else {
                m.value(v, Some(*t), self.version)
            };
        }
        let m = m.finish();
        if self.waiting {
            self.ahead.push_back(m);
        } else {
            self.sent_row();
            self.send(&m)?;
        }
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
    input: &mut BufReader<R>,
    out: &mut W,
) -> io::Result<Ran> {
    let parsed = match session.parse(query.sql) {
        Ok(p) => p,
        Err(e) => return Ok(Ran::Failed(e)),
    };
    if !query.params.is_empty() && parsed.len() > 1 {
        return Ok(Ran::Failed(Error::syntax(
            "a query with parameters must hold one statement",
        )));
    }
    if query.fetch > 0 && parsed.len() > 1 {
        return Ok(Ran::Failed(Error::syntax(
            "a query fetched in batches must hold one statement",
        )));
    }
    // Parameters take the types their places need (text is parsed).
    let mut converted = query.params.to_vec();
    if let [(stmt, text)] = &parsed[..]
        && !query.params.is_empty()
    {
        let types = match session.describe(stmt, Some(text)) {
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
    for (stmt, stmt_text) in parsed.iter() {
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
            waiting: false,
            ahead: VecDeque::new(),
            sent: 0,
            stopped: false,
            copying: None,
            broken: None,
            close: None,
            stepped: query.stepped,
            replied: 0,
        };
        let mut r = session.run_into(stmt, params, &mut wire);
        if wire.broken.is_none() {
            wire.skip_copy()?;
        }
        // The rows read ahead and the last batch, before the end (or the
        // error that ended it).
        if wire.broken.is_none()
            && wire.close.is_none()
            && let Err(e) = wire.finish()
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
        let (command, mut count) = split_tag(&tag);
        if wire.stopped {
            // Rows read ahead and dropped are not the client's.
            count = wire.sent;
        }
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
