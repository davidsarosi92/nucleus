//! A connection: its login (SCRAM-SHA-256 by `saslStart` /
//! `saslContinue`, the server's verifiers), its session, and the
//! commands — each a document whose first key names it — run through
//! `nucleus_sql::ext::documents`, the compiler `ncp`'s `O` message uses.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_kv::Kv;
use nucleus_server::{Config, ScramServer, Stream};
use nucleus_sql::ext::documents::{Op, Request};
use nucleus_sql::ext::{Output, Session, Value};

use crate::bson::{self, B, Doc};
use crate::wire;

/// The wire version claimed: MongoDB 7.0's (OP_MSG; no sessions offered).
const MAX_WIRE_VERSION: i32 = 21;
const MAX_BSON: usize = 16 * 1024 * 1024;

/// Serves one client until it goes away.  A client that starts with a TLS
/// handshake gets TLS when the server has it (`tls=true` in a connection
/// string).
pub fn handle(mut stream: TcpStream, kv: Arc<dyn Kv>, config: &Config, id: u32) -> io::Result<()> {
    let peer = nucleus_server::transport::client_address(&mut stream, &config.trusted_proxies)?;
    let mut first = [0u8; 1];
    if stream.peek(&mut first)? == 0 {
        return Ok(());
    }
    if first[0] == nucleus_server::transport::TLS_HANDSHAKE {
        let Some(tls) = &config.tls else {
            return Ok(());
        };
        let secured = tls.accept(stream)?;
        return serve(secured.stream, kv, config, id, peer, true);
    }
    serve(Box::new(stream), kv, config, id, peer, false)
}

/// Serves one client over any byte stream (a connection made in the
/// process, a fuzzer's bytes): no TLS, no client address.
pub fn handle_stream(
    stream: Box<dyn Stream>,
    kv: Arc<dyn Kv>,
    config: &Config,
    id: u32,
) -> io::Result<()> {
    serve(stream, kv, config, id, None, false)
}

fn serve(
    stream: Box<dyn Stream>,
    kv: Arc<dyn Kv>,
    config: &Config,
    id: u32,
    peer: Option<IpAddr>,
    tls: bool,
) -> io::Result<()> {
    let (r, w) = nucleus_server::transport::split(stream);
    let mut input = BufReader::with_capacity(64 << 10, r);
    let mut out = BufWriter::with_capacity(64 << 10, w);
    let mut conn = Conn {
        kv,
        config,
        id,
        peer,
        tls,
        session: None,
        user: None,
        scram: None,
        cursors: HashMap::new(),
        next_cursor: (i64::from(id) << 32) | 1,
        databases: Vec::new(),
    };
    static REQUESTS: AtomicU32 = AtomicU32::new(1);
    while let Some(m) = wire::read(&mut input)? {
        let rid = REQUESTS.fetch_add(1, Ordering::Relaxed) as i32;
        match m.opcode {
            wire::OP_MSG => {
                let (reply, quiet) = match wire::op_msg(&m.body) {
                    Ok((cmd, quiet)) => (conn.command(&cmd), quiet),
                    Err(e) => (error(2, "BadValue", &e.0), false),
                };
                if !quiet {
                    wire::write_msg(&mut out, rid, m.request_id, &reply)?;
                }
            }
            wire::OP_QUERY => {
                let reply = match wire::op_query(&m.body) {
                    Ok((ns, cmd)) if ns.ends_with(".$cmd") => {
                        let mut cmd = cmd;
                        if bson::get(&cmd, "$db").is_none() {
                            let db = ns.trim_end_matches(".$cmd").to_string();
                            cmd.push(("$db".into(), B::Str(db)));
                        }
                        conn.command(&cmd)
                    }
                    Ok(_) => error(
                        352,
                        "UnsupportedOpQueryCommand",
                        "legacy queries are not supported: send OP_MSG",
                    ),
                    Err(e) => error(2, "BadValue", &e.0),
                };
                let failed = reply.iter().any(|(k, v)| k == "ok" && !v.truthy());
                wire::write_reply(&mut out, rid, m.request_id, &reply, failed)?;
            }
            op => {
                conn.log("error", &format!("opcode {op} is not supported"));
                out.flush()?;
                return Ok(());
            }
        }
        out.flush()?;
    }
    Ok(())
}

struct Cursor {
    ns: String,
    docs: VecDeque<Doc>,
}

struct Conn<'c> {
    kv: Arc<dyn Kv>,
    config: &'c Config,
    id: u32,
    peer: Option<IpAddr>,
    tls: bool,
    session: Option<Session>,
    user: Option<String>,
    /// A login under way: its exchange, its user, and whether the client
    /// skips the empty last round (`skipEmptyExchange`).
    scram: Option<(ScramServer, String, bool, bool)>,
    cursors: HashMap<i64, Cursor>,
    next_cursor: i64,
    /// The database names this connection's commands gave, which
    /// `listDatabases` lists (every name reaches the same collections).
    databases: Vec<String>,
}

/// A command's failure: MongoDB's code and name, and the message.
struct Fail(i32, &'static str, String);

type Reply = Result<Doc, Fail>;

fn error(code: i32, name: &str, message: &str) -> Doc {
    vec![
        ("ok".into(), B::Double(0.0)),
        ("errmsg".into(), B::Str(message.into())),
        ("code".into(), B::Int32(code)),
        ("codeName".into(), B::Str(name.into())),
    ]
}

fn ok(mut d: Doc) -> Reply {
    d.push(("ok".into(), B::Double(1.0)));
    Ok(d)
}

fn bad_value(message: impl Into<String>) -> Fail {
    Fail(2, "BadValue", message.into())
}

/// A nucleus error as MongoDB's: the codes its clients act on (11000 a
/// duplicate key, 26 no such collection, 112 a write conflict), else the
/// SQLSTATE in the message.
fn from_sql(e: &nucleus_sql::ext::Error) -> Fail {
    let (code, name) = match e.code {
        "23505" => (11000, "DuplicateKey"),
        "42P01" => (26, "NamespaceNotFound"),
        "42P07" => (48, "NamespaceExists"),
        "40001" | "40P01" => (112, "WriteConflict"),
        "28000" | "28P01" | "42501" => (13, "Unauthorized"),
        "22023" | "0A000" | "42601" | "22P02" | "22032" => (2, "BadValue"),
        "53300" => (6, "HostUnreachable"),
        _ => (1, "InternalError"),
    };
    Fail(code, name, format!("{} ({})", e.message, e.code))
}

/// A nucleus `json` value's text.
fn json_text(v: &Value) -> Option<String> {
    match v {
        Value::Ext(t, b) if t.name() == "json" => nucleus_sql::ext::json::to_text(b).ok(),
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A document from nucleus, `_id` first as MongoDB keeps it.
fn document(text: &str) -> Result<Doc, Fail> {
    match bson::from_json(text) {
        Ok(B::Doc(mut d)) => {
            if let Some(i) = d.iter().position(|(k, _)| k == "_id")
                && i > 0
            {
                let id = d.remove(i);
                d.insert(0, id);
            }
            Ok(d)
        }
        Ok(_) => Err(Fail(
            1,
            "InternalError",
            "a document that is not an object".into(),
        )),
        Err(e) => Err(Fail(1, "InternalError", e.0)),
    }
}

fn value_of(v: &Value) -> B {
    match v {
        Value::Null => B::Null,
        Value::Int(i) => i32::try_from(*i).map_or(B::Int64(*i), B::Int32),
        Value::Bool(b) => B::Bool(*b),
        v => json_text(v)
            .and_then(|t| bson::from_json(&t).ok())
            .unwrap_or(B::Null),
    }
}

fn int(v: Option<&B>) -> i64 {
    v.and_then(B::as_i64).unwrap_or(0)
}

/// A filter as JSON for the compiler: a regular expression as `$regex`, and
/// an ordering comparison with a date or an ObjectId as one on its
/// Extended JSON text (`{"at": {"$gt": {"$date": …}}}` as `{"at.$date":
/// {"$gt": "…"}}`) — the ISO text and the hex sort as the values do.
fn filter_json(f: Option<&B>) -> Result<String, Fail> {
    match f {
        None | Some(B::Null) => Ok(String::new()),
        Some(B::Doc(d)) => Ok(bson::doc_json(&filter_doc(d), true)),
        Some(_) => Err(bad_value("a filter must be a document")),
    }
}

/// A date's or an ObjectId's Extended JSON tag and text (a date outside
/// the years 0–9999, kept as `$numberLong`, has none: its text does not
/// sort).
fn wrapped(v: &B) -> Option<(&'static str, B)> {
    match v {
        B::Date(ms) => bson::iso(*ms).map(|t| ("$date", B::Str(t))),
        B::ObjectId(o) => Some((
            "$oid",
            B::Str(o.iter().map(|b| format!("{b:02x}")).collect()),
        )),
        _ => None,
    }
}

/// Whether a value is one Extended JSON writes as a `$`-object.
fn extended(v: &B) -> bool {
    match v {
        B::Double(f) => !f.is_finite(),
        B::Binary(..)
        | B::ObjectId(_)
        | B::Date(_)
        | B::Timestamp(..)
        | B::Decimal(_)
        | B::MinKey
        | B::MaxKey => true,
        _ => false,
    }
}

fn filter_doc(d: &Doc) -> Doc {
    let mut out: Doc = Vec::new();
    let mut extra: Vec<B> = Vec::new();
    for (k, v) in d {
        match (k.as_str(), v) {
            ("$and" | "$or" | "$nor", B::Arr(a)) => {
                let parts = a
                    .iter()
                    .map(|x| match x {
                        B::Doc(d) => B::Doc(filter_doc(d)),
                        other => other.clone(),
                    })
                    .collect();
                out.push((k.clone(), B::Arr(parts)));
            }
            (field, B::Doc(ops))
                if !field.starts_with('$')
                    && ops.first().is_some_and(|(o, _)| o.starts_with('$')) =>
            {
                let mut kept: Doc = Vec::new();
                for (op, x) in ops {
                    match (op.as_str(), x) {
                        ("$gt" | "$gte" | "$lt" | "$lte", x) if wrapped(x).is_some() => {
                            let (tag, text) = wrapped(x).unwrap();
                            extra.push(B::Doc(vec![(
                                format!("{field}.{tag}"),
                                B::Doc(vec![(op.clone(), text)]),
                            )]));
                        }
                        ("$elemMatch", B::Doc(inner)) => {
                            kept.push((op.clone(), B::Doc(filter_doc(inner))))
                        }
                        ("$not", B::Doc(inner)) => {
                            let wrapped =
                                filter_doc(&vec![(field.to_string(), B::Doc(inner.clone()))]);
                            let inner = wrapped
                                .into_iter()
                                .next()
                                .map(|(_, v)| v)
                                .unwrap_or(B::Null);
                            kept.push((op.clone(), inner));
                        }
                        _ => kept.push((op.clone(), x.clone())),
                    }
                }
                if !kept.is_empty() {
                    out.push((k.clone(), B::Doc(kept)));
                }
            }
            // A value Extended JSON wraps (`{"$oid": …}`) is an operand,
            // not operators: equality with it, exact.
            (field, v) if !field.starts_with('$') && extended(v) => {
                out.push((k.clone(), B::Doc(vec![("$eq".into(), v.clone())])));
            }
            _ => out.push((k.clone(), v.clone())),
        }
    }
    if !extra.is_empty() {
        if !out.is_empty() {
            extra.insert(0, B::Doc(out));
        }
        return vec![("$and".into(), B::Arr(extra))];
    }
    out
}

/// A fresh ObjectId: seconds, 5 random bytes (once per process), a counter.
fn object_id(kv: &dyn Kv) -> B {
    static RANDOM: std::sync::OnceLock<[u8; 8]> = std::sync::OnceLock::new();
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let r = RANDOM.get_or_init(|| {
        let mut b = [0u8; 8];
        kv.random_bytes(&mut b);
        COUNTER.store(u32::from_le_bytes([b[5], b[6], b[7], 0]), Ordering::Relaxed);
        b
    });
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut o = [0u8; 12];
    o[0..4].copy_from_slice(&secs.to_be_bytes());
    o[4..9].copy_from_slice(&r[0..5]);
    o[9..12].copy_from_slice(&c.to_be_bytes()[1..4]);
    B::ObjectId(o)
}

/// The user a SCRAM client-first message names (`n,,n=<user>,r=…`).
fn scram_user(first: &str) -> Option<String> {
    let bare = first.splitn(3, ',').nth(2)?;
    let n = bare.split(',').find_map(|p| p.strip_prefix("n="))?;
    Some(n.replace("=2C", ",").replace("=3D", "="))
}

fn binary(v: Option<&B>) -> Vec<u8> {
    match v {
        Some(B::Binary(_, b)) => b.clone(),
        Some(B::Str(s)) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Conn<'_> {
    fn log(&self, what: &str, text: &str) {
        if self.config.log_queries {
            nucleus_server::log!(
                "nucleus-server: [{}] mongo {what}: {}",
                self.id,
                text.trim()
            );
        }
    }

    /// Runs one command; its reply (`ok: 0` and the error when it fails).
    fn command(&mut self, cmd: &Doc) -> Doc {
        let Some((name, _)) = cmd.first() else {
            return error(59, "CommandNotFound", "an empty command");
        };
        let name = name.clone();
        let db = bson::get(cmd, "$db")
            .and_then(B::as_str)
            .unwrap_or("test")
            .to_string();
        if !matches!(db.as_str(), "admin" | "config" | "local") && !self.databases.contains(&db) {
            self.databases.push(db.clone());
        }
        self.log("command", &format!("{name} {}", bson::doc_json(cmd, false)));
        let open = matches!(
            name.as_str(),
            "hello"
                | "isMaster"
                | "ismaster"
                | "saslStart"
                | "saslContinue"
                | "ping"
                | "buildInfo"
                | "buildinfo"
                | "endSessions"
                | "whatsmyuri"
                | "atlasVersion"
                | "getnonce"
                | "logout"
        );
        if !open
            && self.session.is_none()
            && !self.config.auth.wants_password()
            && let Err(e) = self.open_session(nucleus_server::DEFAULT_USER.to_string(), "trust")
        {
            return error(e.0, e.1, &e.2);
        }
        if !open && self.session.is_none() {
            return error(
                13,
                "Unauthorized",
                &format!("command {name} requires authentication"),
            );
        }
        let r = match name.as_str() {
            "hello" | "isMaster" | "ismaster" => self.hello(cmd, name != "hello"),
            "ping" | "endSessions" | "logout" => ok(Vec::new()),
            "buildInfo" | "buildinfo" => ok(vec![
                ("version".into(), B::Str("7.0.0".into())),
                ("gitVersion".into(), B::Str("nucleus".into())),
                (
                    "versionArray".into(),
                    B::Arr(vec![B::Int32(7), B::Int32(0), B::Int32(0), B::Int32(0)]),
                ),
                ("bits".into(), B::Int32(64)),
                ("maxBsonObjectSize".into(), B::Int32(MAX_BSON as i32)),
                ("modules".into(), B::Arr(Vec::new())),
            ]),
            "whatsmyuri" => ok(vec![(
                "you".into(),
                B::Str(self.peer.map_or("unknown".into(), |p| p.to_string())),
            )]),
            "saslStart" => self.sasl_start(cmd),
            "saslContinue" => self.sasl_continue(cmd),
            "connectionStatus" => {
                let user = self.user.clone().unwrap_or_default();
                ok(vec![(
                    "authInfo".into(),
                    B::Doc(vec![
                        (
                            "authenticatedUsers".into(),
                            B::Arr(vec![B::Doc(vec![
                                ("user".into(), B::Str(user)),
                                ("db".into(), B::Str("admin".into())),
                            ])]),
                        ),
                        ("authenticatedUserRoles".into(), B::Arr(Vec::new())),
                    ]),
                )])
            }
            "getParameter" => ok(vec![(
                "featureCompatibilityVersion".into(),
                B::Doc(vec![("version".into(), B::Str("7.0".into()))]),
            )]),
            "getCmdLineOpts" => ok(vec![
                ("argv".into(), B::Arr(vec![B::Str("nucleus".into())])),
                ("parsed".into(), B::Doc(Vec::new())),
            ]),
            "getLog" => ok(vec![
                ("totalLinesWritten".into(), B::Int32(0)),
                ("log".into(), B::Arr(Vec::new())),
            ]),
            "hostInfo" => ok(vec![(
                "system".into(),
                B::Doc(vec![("hostname".into(), B::Str("nucleus".into()))]),
            )]),
            "listDatabases" => {
                let names = if self.databases.is_empty() {
                    vec!["test".to_string()]
                } else {
                    self.databases.clone()
                };
                ok(vec![
                    (
                        "databases".into(),
                        B::Arr(
                            names
                                .into_iter()
                                .map(|n| {
                                    B::Doc(vec![
                                        ("name".into(), B::Str(n)),
                                        ("sizeOnDisk".into(), B::Int64(0)),
                                        ("empty".into(), B::Bool(false)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                    ("totalSize".into(), B::Int64(0)),
                ])
            }
            "listCollections" => self.list_collections(cmd, &db),
            "create" => self.create(cmd),
            "drop" => self.drop(cmd, &db),
            "find" => self.find(cmd, &db),
            "getMore" => self.get_more(cmd),
            "killCursors" => self.kill_cursors(cmd),
            "insert" => self.insert(cmd),
            "update" => self.update(cmd),
            "delete" => self.delete(cmd),
            "count" => self.count(cmd),
            "aggregate" => self.aggregate(cmd, &db),
            "findAndModify" | "findandmodify" => self.find_and_modify(cmd),
            "createIndexes" => self.create_indexes(cmd),
            "listIndexes" => self.list_indexes(cmd, &db),
            "dropIndexes" => self.drop_indexes(cmd),
            other => Err(Fail(
                59,
                "CommandNotFound",
                format!(
                    "no such command: '{other}' (nucleus's MongoDB compatibility runs CRUD and queries)"
                ),
            )),
        };
        match r {
            Ok(d) => d,
            Err(Fail(code, name, message)) => {
                self.log("error", &format!("{code} {message}"));
                error(code, name, &message)
            }
        }
    }

    fn hello(&self, cmd: &Doc, legacy: bool) -> Reply {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let mut d: Doc = vec![
            (
                if legacy {
                    "ismaster"
                } else {
                    "isWritablePrimary"
                }
                .into(),
                B::Bool(true),
            ),
            ("helloOk".into(), B::Bool(true)),
            ("maxBsonObjectSize".into(), B::Int32(MAX_BSON as i32)),
            (
                "maxMessageSizeBytes".into(),
                B::Int32(wire::MAX_MESSAGE as i32),
            ),
            ("maxWriteBatchSize".into(), B::Int32(100_000)),
            ("localTime".into(), B::Date(now)),
            ("connectionId".into(), B::Int32(self.id as i32)),
            ("minWireVersion".into(), B::Int32(0)),
            ("maxWireVersion".into(), B::Int32(MAX_WIRE_VERSION)),
            ("readOnly".into(), B::Bool(false)),
        ];
        if bson::get(cmd, "saslSupportedMechs").is_some() {
            d.push((
                "saslSupportedMechs".into(),
                B::Arr(vec![B::Str("SCRAM-SHA-256".into())]),
            ));
        }
        ok(d)
    }

    // ------------------------------------------------------------ login

    fn sasl_start(&mut self, cmd: &Doc) -> Reply {
        let mechanism = bson::get(cmd, "mechanism")
            .and_then(B::as_str)
            .unwrap_or("");
        if mechanism != "SCRAM-SHA-256" {
            return Err(Fail(
                334,
                "MechanismUnavailable",
                format!("{mechanism} is not offered: SCRAM-SHA-256 is"),
            ));
        }
        let first = String::from_utf8_lossy(&binary(bson::get(cmd, "payload"))).into_owned();
        let Some(user) = scram_user(&first) else {
            return Err(Fail(
                18,
                "AuthenticationFailed",
                "Authentication failed.".into(),
            ));
        };
        if let Some(why) = self.config.throttle.locked(&*self.kv, &user) {
            return Err(Fail(18, "AuthenticationFailed", why));
        }
        if nucleus_server::directory_user(self.config, &*self.kv, &user) {
            return Err(Fail(
                18,
                "AuthenticationFailed",
                format!(
                    "the password of \"{user}\" is checked by a directory: log in over ncp or PostgreSQL's protocol"
                ),
            ));
        }
        let mut server = nucleus_server::scram_server(self.config, &*self.kv, &user);
        let skip = bson::get(cmd, "options")
            .and_then(B::as_doc)
            .and_then(|o| bson::get(o, "skipEmptyExchange"))
            .is_some_and(B::truthy);
        match server.first(&first) {
            Ok(server_first) => {
                self.scram = Some((server, user, skip, false));
                ok(vec![
                    ("conversationId".into(), B::Int32(1)),
                    ("done".into(), B::Bool(false)),
                    ("payload".into(), B::Binary(0, server_first.into_bytes())),
                ])
            }
            Err(_) => Err(self.refused(&user)),
        }
    }

    fn sasl_continue(&mut self, cmd: &Doc) -> Reply {
        let Some((server, user, skip, proved)) = self.scram.take() else {
            return Err(Fail(
                17,
                "ProtocolError",
                "no SASL conversation is under way".into(),
            ));
        };
        if proved {
            // The empty last round of a client that does not skip it.
            return self.logged_in(user, Vec::new());
        }
        let last = String::from_utf8_lossy(&binary(bson::get(cmd, "payload"))).into_owned();
        match server.finish(&last) {
            Ok(server_final) => {
                if skip {
                    self.logged_in(user, server_final.into_bytes())
                } else {
                    self.scram = Some((server, user, skip, true));
                    ok(vec![
                        ("conversationId".into(), B::Int32(1)),
                        ("done".into(), B::Bool(false)),
                        ("payload".into(), B::Binary(0, server_final.into_bytes())),
                    ])
                }
            }
            Err(_) => Err(self.refused(&user)),
        }
    }

    fn refused(&self, user: &str) -> Fail {
        self.config.throttle.failed(&*self.kv, user, self.peer);
        let _ = nucleus_sql::ext::audit_refused_login(
            &self.kv,
            user,
            self.context(nucleus_server::password_method(
                self.config,
                &*self.kv,
                user,
            )),
            "28P01",
            &format!("password authentication failed for user \"{user}\""),
        );
        Fail(18, "AuthenticationFailed", "Authentication failed.".into())
    }

    fn context(&self, method: &str) -> nucleus_sql::ext::Context {
        nucleus_sql::ext::Context {
            client_addr: self.peer,
            front_end: "mongo".into(),
            auth_method: method.into(),
            tls: self.tls,
        }
    }

    fn logged_in(&mut self, user: String, payload: Vec<u8>) -> Reply {
        self.config.throttle.succeeded(&user);
        let method = nucleus_server::password_method(self.config, &*self.kv, &user);
        self.open_session(user, method)?;
        ok(vec![
            ("conversationId".into(), B::Int32(1)),
            ("done".into(), B::Bool(true)),
            ("payload".into(), B::Binary(0, payload)),
        ])
    }

    fn open_session(&mut self, user: String, method: &str) -> Result<(), Fail> {
        let mut session =
            Session::with_registry(Arc::clone(&self.kv), Arc::clone(&self.config.registry));
        session
            .login(&user)
            .and_then(|()| nucleus_server::room_for(self.config, &self.kv, &session))
            .and_then(|()| session.admit(self.context(method)))
            .map_err(|e| from_sql(&e))?;
        self.session = Some(session);
        self.user = Some(user);
        Ok(())
    }

    // ------------------------------------------------------- statements

    fn sql(&mut self, sql: &str) -> Result<Output, nucleus_sql::ext::Error> {
        self.log("statement", sql);
        let session = self.session.as_mut().expect("logged in");
        let mut out = session.execute(sql)?;
        Ok(out.pop().unwrap_or(Output::Command(String::new())))
    }

    /// Runs a request; its rows, or the count a command tag gives.
    fn request(&mut self, r: &Request) -> Result<(Vec<Vec<Value>>, u64), nucleus_sql::ext::Error> {
        let sql = nucleus_sql::ext::documents::statement(r)?;
        match self.sql(&sql)? {
            Output::Rows { rows, .. } => {
                let n = rows.len() as u64;
                Ok((rows, n))
            }
            Output::Command(tag) => Ok((
                Vec::new(),
                tag.rsplit(' ')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
            )),
        }
    }

    /// The same, making the collection first when it is not there and the
    /// request writes (MongoDB makes a collection on its first write).
    fn writing(&mut self, r: &Request) -> Result<(Vec<Vec<Value>>, u64), nucleus_sql::ext::Error> {
        match self.request(r) {
            Err(e) if e.code == "42P01" => {
                self.sql(&format!(
                    "CREATE COLLECTION IF NOT EXISTS {}",
                    ident(&r.collection)
                ))?;
                self.request(r)
            }
            other => other,
        }
    }

    /// The same for a read: a collection that is not there has no
    /// documents.
    fn reading(&mut self, r: &Request) -> Result<(Vec<Vec<Value>>, u64), nucleus_sql::ext::Error> {
        match self.request(r) {
            Err(e) if e.code == "42P01" => Ok((Vec::new(), 0)),
            other => other,
        }
    }

    fn collection(cmd: &Doc) -> Result<String, Fail> {
        match cmd.first() {
            Some((_, B::Str(c))) if !c.is_empty() => Ok(c.clone()),
            _ => Err(Fail(
                73,
                "InvalidNamespace",
                "the collection's name is missing".into(),
            )),
        }
    }

    fn create(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        self.sql(&format!("CREATE COLLECTION {}", ident(&c)))
            .map_err(|e| from_sql(&e))?;
        ok(Vec::new())
    }

    fn drop(&mut self, cmd: &Doc, db: &str) -> Reply {
        let c = Self::collection(cmd)?;
        self.sql(&format!("DROP COLLECTION IF EXISTS {}", ident(&c)))
            .map_err(|e| from_sql(&e))?;
        ok(vec![("ns".into(), B::Str(format!("{db}.{c}")))])
    }

    fn list_collections(&mut self, cmd: &Doc, db: &str) -> Reply {
        let names = match self
            .sql("SELECT name FROM nc_catalog.collections ORDER BY name")
            .map_err(|e| from_sql(&e))?
        {
            Output::Rows { rows, .. } => rows
                .iter()
                .filter_map(|r| match r.first() {
                    Some(Value::Text(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Output::Command(_) => Vec::new(),
        };
        let wanted = bson::get(cmd, "filter")
            .and_then(B::as_doc)
            .and_then(|f| bson::get(f, "name"))
            .cloned();
        let name_only = bson::get(cmd, "nameOnly").is_some_and(B::truthy);
        let docs: Vec<Doc> = names
            .into_iter()
            .filter(|n| match &wanted {
                Some(B::Str(w)) => w == n,
                Some(B::Regex(p, _)) => n.contains(p.trim_start_matches('^').trim_end_matches('$')),
                _ => true,
            })
            .map(|n| {
                let mut d: Doc = vec![
                    ("name".into(), B::Str(n)),
                    ("type".into(), B::Str("collection".into())),
                ];
                if !name_only {
                    d.push(("options".into(), B::Doc(Vec::new())));
                    d.push((
                        "info".into(),
                        B::Doc(vec![("readOnly".into(), B::Bool(false))]),
                    ));
                    d.push((
                        "idIndex".into(),
                        B::Doc(vec![
                            ("v".into(), B::Int32(2)),
                            ("key".into(), B::Doc(vec![("_id".into(), B::Int32(1))])),
                            ("name".into(), B::Str("_id_".into())),
                        ]),
                    ));
                }
                d
            })
            .collect();
        self.cursor_reply(
            format!("{db}.$cmd.listCollections"),
            docs.into(),
            None,
            true,
        )
    }

    /// A cursor's first batch: at most `batch` documents (101 by default)
    /// and 16 MiB; the rest kept for `getMore` unless `single`.
    fn cursor_reply(
        &mut self,
        ns: String,
        mut docs: VecDeque<Doc>,
        batch: Option<i64>,
        single: bool,
    ) -> Reply {
        let first = take_batch(&mut docs, batch.filter(|&b| b > 0).unwrap_or(101) as usize);
        let id = if docs.is_empty() || single {
            0
        } else {
            let id = self.next_cursor;
            self.next_cursor += 1;
            self.cursors.insert(
                id,
                Cursor {
                    ns: ns.clone(),
                    docs,
                },
            );
            id
        };
        ok(vec![(
            "cursor".into(),
            B::Doc(vec![
                ("firstBatch".into(), B::Arr(first)),
                ("id".into(), B::Int64(id)),
                ("ns".into(), B::Str(ns)),
            ]),
        )])
    }

    fn get_more(&mut self, cmd: &Doc) -> Reply {
        let id = int(cmd.first().map(|(_, v)| v));
        let batch = bson::get(cmd, "batchSize")
            .and_then(B::as_i64)
            .filter(|&b| b > 0);
        let Some(mut c) = self.cursors.remove(&id) else {
            return Err(Fail(
                43,
                "CursorNotFound",
                format!("cursor id {id} not found"),
            ));
        };
        let next = take_batch(&mut c.docs, batch.map_or(usize::MAX, |b| b as usize));
        let ns = c.ns.clone();
        let id = if c.docs.is_empty() {
            0
        } else {
            self.cursors.insert(id, c);
            id
        };
        ok(vec![(
            "cursor".into(),
            B::Doc(vec![
                ("nextBatch".into(), B::Arr(next)),
                ("id".into(), B::Int64(id)),
                ("ns".into(), B::Str(ns)),
            ]),
        )])
    }

    fn kill_cursors(&mut self, cmd: &Doc) -> Reply {
        let (mut killed, mut missing) = (Vec::new(), Vec::new());
        if let Some(B::Arr(ids)) = bson::get(cmd, "cursors") {
            for id in ids.iter().filter_map(B::as_i64) {
                if self.cursors.remove(&id).is_some() {
                    killed.push(B::Int64(id));
                } else {
                    missing.push(B::Int64(id));
                }
            }
        }
        ok(vec![
            ("cursorsKilled".into(), B::Arr(killed)),
            ("cursorsNotFound".into(), B::Arr(missing)),
            ("cursorsAlive".into(), B::Arr(Vec::new())),
            ("cursorsUnknown".into(), B::Arr(Vec::new())),
        ])
    }

    fn docs_of(rows: &[Vec<Value>]) -> Result<VecDeque<Doc>, Fail> {
        rows.iter()
            .map(|r| document(&r.first().and_then(json_text).unwrap_or_else(|| "{}".into())))
            .collect()
    }

    fn json_part(v: Option<&B>) -> Result<String, Fail> {
        match v {
            None | Some(B::Null) => Ok(String::new()),
            Some(B::Doc(d)) if d.is_empty() => Ok(String::new()),
            Some(B::Doc(d)) => Ok(bson::doc_json(d, false)),
            Some(_) => Err(bad_value("a document expected")),
        }
    }

    fn find(&mut self, cmd: &Doc, db: &str) -> Reply {
        let c = Self::collection(cmd)?;
        let limit = int(bson::get(cmd, "limit"));
        let single = bson::get(cmd, "singleBatch").is_some_and(B::truthy) || limit < 0;
        let r = Request {
            op: Some(Op::Find),
            collection: c.clone(),
            filter: filter_json(bson::get(cmd, "filter"))?,
            body: Self::json_part(bson::get(cmd, "projection"))?,
            sort: Self::json_part(bson::get(cmd, "sort"))?,
            skip: int(bson::get(cmd, "skip")).max(0) as u64,
            limit: limit.unsigned_abs(),
            ..Request::default()
        };
        let (rows, _) = self.reading(&r).map_err(|e| from_sql(&e))?;
        let docs = Self::docs_of(&rows)?;
        let batch = bson::get(cmd, "batchSize").and_then(B::as_i64);
        self.cursor_reply(format!("{db}.{c}"), docs, batch, single)
    }

    fn count(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let n = self.count_of(
            &c,
            bson::get(cmd, "query"),
            int(bson::get(cmd, "skip")),
            int(bson::get(cmd, "limit")),
        )?;
        ok(vec![("n".into(), n)])
    }

    fn count_of(&mut self, c: &str, filter: Option<&B>, skip: i64, limit: i64) -> Result<B, Fail> {
        let r = Request {
            op: Some(Op::Count),
            collection: c.to_string(),
            filter: filter_json(filter)?,
            skip: skip.max(0) as u64,
            limit: limit.unsigned_abs(),
            ..Request::default()
        };
        let (rows, _) = self.reading(&r).map_err(|e| from_sql(&e))?;
        Ok(rows
            .first()
            .and_then(|r| r.first())
            .map_or(B::Int32(0), value_of))
    }

    fn insert(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let ordered = bson::get(cmd, "ordered").is_none_or(B::truthy);
        let Some(B::Arr(docs)) = bson::get(cmd, "documents") else {
            return Err(bad_value("insert takes documents"));
        };
        let mut texts = Vec::with_capacity(docs.len());
        for d in docs {
            let B::Doc(d) = d else {
                return Err(bad_value("insert takes documents"));
            };
            let mut d = d.clone();
            if bson::get(&d, "_id").is_none() {
                d.insert(0, ("_id".into(), object_id(&*self.kv)));
            }
            texts.push(bson::doc_json(&d, false));
        }
        if texts.is_empty() {
            return Err(bad_value("insert takes at least one document"));
        }
        let one = |body: String| Request {
            op: Some(Op::Insert),
            collection: c.clone(),
            body,
            ..Request::default()
        };
        // All at once, one statement; if that fails, one by one, so the
        // documents before the failing one stay (ordered) or every other
        // one does (unordered), as MongoDB's writes do.
        let all = one(format!("[{}]", texts.join(", ")));
        if let Ok((_, n)) = self.writing(&all) {
            return ok(vec![("n".into(), B::Int32(n as i32))]);
        }
        let (mut n, mut errors) = (0, Vec::new());
        for (i, t) in texts.into_iter().enumerate() {
            match self.writing(&one(t)) {
                Ok(_) => n += 1,
                Err(e) => {
                    errors.push(write_error(i, &from_sql(&e)));
                    if ordered {
                        break;
                    }
                }
            }
        }
        let mut d = vec![("n".into(), B::Int32(n))];
        if !errors.is_empty() {
            d.push(("writeErrors".into(), B::Arr(errors)));
        }
        ok(d)
    }

    /// One update statement (`q`, `u`, `upsert`, `multi`): matched,
    /// modified, the upserted `_id`.
    fn update_one(&mut self, c: &str, u: &Doc) -> Result<(i64, i64, Option<B>), Fail> {
        let q = bson::get(u, "q");
        let body = match bson::get(u, "u") {
            Some(B::Doc(d)) => d.clone(),
            Some(B::Arr(_)) => {
                return Err(bad_value(
                    "update pipelines are not supported: an update document is",
                ));
            }
            _ => return Err(bad_value("an update needs u")),
        };
        if bson::get(u, "arrayFilters").is_some() {
            return Err(bad_value("arrayFilters are not supported"));
        }
        let upsert = bson::get(u, "upsert").is_some_and(B::truthy);
        let many = bson::get(u, "multi").is_some_and(B::truthy);
        let operators = body.first().is_some_and(|(k, _)| k.starts_with('$'));
        let filter = filter_json(q)?;
        let id_given = q
            .and_then(B::as_doc)
            .and_then(|q| bson::get(q, "_id"))
            .is_some_and(
                |v| !matches!(v, B::Doc(d) if d.first().is_some_and(|(k, _)| k.starts_with('$'))),
            );
        let mut r = Request {
            op: Some(if operators { Op::Update } else { Op::Replace }),
            collection: c.to_string(),
            filter,
            body: bson::doc_json(&body, false),
            many,
            upsert,
            ..Request::default()
        };
        // An upsert's new document gets an ObjectId, as MongoDB's do, where
        // neither the filter nor the update gives the _id.
        if upsert && !id_given {
            if operators {
                let sets_id = body
                    .iter()
                    .any(|(_, v)| v.as_doc().is_some_and(|d| bson::get(d, "_id").is_some()));
                if !sets_id {
                    let mut with = body.clone();
                    match with.iter_mut().find(|(k, _)| k == "$setOnInsert") {
                        Some((_, B::Doc(d))) => d.push(("_id".into(), object_id(&*self.kv))),
                        _ => with.push((
                            "$setOnInsert".into(),
                            B::Doc(vec![("_id".into(), object_id(&*self.kv))]),
                        )),
                    }
                    r.body = bson::doc_json(&with, false);
                }
            } else if bson::get(&body, "_id").is_none() {
                // A replacement: the match keeps its _id, so it is tried
                // first; the insert, if none, gets one.
                let mut plain = r.clone();
                plain.upsert = false;
                let (rows, _) = self.reading(&plain).map_err(|e| from_sql(&e))?;
                let row = rows.first().cloned().unwrap_or_default();
                let matched = row
                    .first()
                    .map_or(B::Int32(0), value_of)
                    .as_i64()
                    .unwrap_or(0);
                if matched > 0 {
                    let modified = row
                        .get(1)
                        .map_or(B::Int32(0), value_of)
                        .as_i64()
                        .unwrap_or(0);
                    return Ok((matched, modified, None));
                }
                let mut with = body.clone();
                with.insert(0, ("_id".into(), object_id(&*self.kv)));
                r.body = bson::doc_json(&with, false);
            }
        }
        let (rows, _) = if upsert {
            self.writing(&r)
        } else {
            self.reading(&r)
        }
        .map_err(|e| from_sql(&e))?;
        let row = rows.first().cloned().unwrap_or_default();
        let get = |i: usize| {
            row.get(i)
                .map_or(B::Int32(0), value_of)
                .as_i64()
                .unwrap_or(0)
        };
        let upserted = row.get(2).map(value_of).filter(|v| *v != B::Null);
        Ok((get(0), get(1), upserted))
    }

    fn update(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let ordered = bson::get(cmd, "ordered").is_none_or(B::truthy);
        let Some(B::Arr(updates)) = bson::get(cmd, "updates") else {
            return Err(bad_value("update takes updates"));
        };
        let (mut n, mut modified) = (0i64, 0i64);
        let (mut upserted, mut errors) = (Vec::new(), Vec::new());
        for (i, u) in updates.iter().enumerate() {
            let Some(u) = u.as_doc() else {
                return Err(bad_value("an update is a document"));
            };
            match self.update_one(&c, u) {
                Ok((m, md, id)) => {
                    n += m;
                    modified += md;
                    if let Some(id) = id {
                        n += 1;
                        upserted.push(B::Doc(vec![
                            ("index".into(), B::Int32(i as i32)),
                            ("_id".into(), id),
                        ]));
                    }
                }
                Err(f) => {
                    errors.push(write_error(i, &f));
                    if ordered {
                        break;
                    }
                }
            }
        }
        let mut d = vec![
            ("n".into(), B::Int32(n as i32)),
            ("nModified".into(), B::Int32(modified as i32)),
        ];
        if !upserted.is_empty() {
            d.push(("upserted".into(), B::Arr(upserted)));
        }
        if !errors.is_empty() {
            d.push(("writeErrors".into(), B::Arr(errors)));
        }
        ok(d)
    }

    fn delete(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let ordered = bson::get(cmd, "ordered").is_none_or(B::truthy);
        let Some(B::Arr(deletes)) = bson::get(cmd, "deletes") else {
            return Err(bad_value("delete takes deletes"));
        };
        let (mut n, mut errors) = (0u64, Vec::new());
        for (i, d) in deletes.iter().enumerate() {
            let Some(d) = d.as_doc() else {
                return Err(bad_value("a delete is a document"));
            };
            let r = filter_json(bson::get(d, "q")).map(|filter| Request {
                op: Some(Op::Delete),
                collection: c.clone(),
                filter,
                many: int(bson::get(d, "limit")) == 0,
                ..Request::default()
            });
            match r.and_then(|r| self.reading(&r).map_err(|e| from_sql(&e))) {
                Ok((_, k)) => n += k,
                Err(f) => {
                    errors.push(write_error(i, &f));
                    if ordered {
                        break;
                    }
                }
            }
        }
        let mut d = vec![("n".into(), B::Int32(n as i32))];
        if !errors.is_empty() {
            d.push(("writeErrors".into(), B::Arr(errors)));
        }
        ok(d)
    }

    /// `aggregate` of the pipelines drivers send for queries and counts:
    /// `$match` first, then `$sort`, `$skip`, `$limit`, `$project`, and a
    /// last `$count` or `$group` counting (`countDocuments`) — compiled to a
    /// find or a count.  Other stages are SQL's (PLAN.md §M16).
    fn aggregate(&mut self, cmd: &Doc, db: &str) -> Reply {
        let c = Self::collection(cmd)?;
        let Some(B::Arr(stages)) = bson::get(cmd, "pipeline") else {
            return Err(bad_value("aggregate takes a pipeline"));
        };
        let (mut matches, mut sort, mut project) = (Vec::new(), None, None);
        let (mut skip, mut limit) = (0i64, 0i64);
        let mut counted: Option<(Doc, String)> = None;
        let unsupported = |s: &str| {
            Fail(
                168,
                "InvalidPipelineOperator",
                format!(
                    "{s}: nucleus's MongoDB compatibility runs $match, $sort, $skip, $limit, $project and counts; SQL answers the rest"
                ),
            )
        };
        for (i, st) in stages.iter().enumerate() {
            let Some((name, arg)) = st.as_doc().and_then(|d| d.first()) else {
                return Err(bad_value("a stage is a document"));
            };
            if counted.is_some() {
                return Err(unsupported("a stage after a count"));
            }
            let later = sort.is_some() || skip > 0 || limit > 0 || project.is_some();
            match (name.as_str(), arg) {
                ("$match", B::Doc(m)) if !later => matches.push(B::Doc(m.clone())),
                ("$sort", B::Doc(s)) if skip == 0 && limit == 0 && project.is_none() => {
                    sort = Some(s.clone())
                }
                ("$skip", n) => {
                    let n = n.as_i64().unwrap_or(0).max(0);
                    skip += n;
                    if limit > 0 {
                        limit = (limit - n).max(0);
                        if limit == 0 {
                            limit = -1;
                        }
                    }
                }
                ("$limit", n) => {
                    let n = n.as_i64().unwrap_or(0).max(1);
                    limit = if limit > 0 {
                        limit.min(n)
                    } else if limit < 0 {
                        -1
                    } else {
                        n
                    };
                }
                ("$project", B::Doc(p)) if project.is_none() => project = Some(p.clone()),
                ("$count", B::Str(field)) => counted = Some((Vec::new(), field.clone())),
                ("$group", B::Doc(g)) => {
                    let id = bson::get(g, "_id").cloned().unwrap_or(B::Null);
                    let counts = g.len() == 2
                        && g.iter().any(|(k, v)| {
                            k != "_id"
                                && v.as_doc().is_some_and(|s| {
                                    s.len() == 1 && s[0].0 == "$sum" && s[0].1.as_i64() == Some(1)
                                })
                        });
                    if !counts || matches!(id, B::Str(ref s) if s.starts_with('$')) {
                        return Err(unsupported("$group other than a count"));
                    }
                    let field = g
                        .iter()
                        .find(|(k, _)| k != "_id")
                        .map(|(k, _)| k.clone())
                        .unwrap();
                    counted = Some((vec![("_id".into(), id)], field));
                }
                (other, _) => return Err(unsupported(&format!("{other} (stage {i})"))),
            }
        }
        let filter = match matches.len() {
            0 => None,
            1 => matches.pop(),
            _ => Some(B::Doc(vec![("$and".into(), B::Arr(matches))])),
        };
        let docs: VecDeque<Doc> = if limit < 0 {
            VecDeque::new()
        } else if let Some((mut head, field)) = counted {
            let n = self.count_of(&c, filter.as_ref(), skip, limit)?;
            if n.as_i64() == Some(0) {
                VecDeque::new()
            } else {
                head.push((field, n));
                VecDeque::from([head])
            }
        } else {
            let r = Request {
                op: Some(Op::Find),
                collection: c.clone(),
                filter: filter_json(filter.as_ref())?,
                body: Self::json_part(project.map(B::Doc).as_ref())?,
                sort: Self::json_part(sort.map(B::Doc).as_ref())?,
                skip: skip as u64,
                limit: limit as u64,
                ..Request::default()
            };
            let (rows, _) = self.reading(&r).map_err(|e| from_sql(&e))?;
            Self::docs_of(&rows)?
        };
        let batch = bson::get(cmd, "cursor")
            .and_then(B::as_doc)
            .and_then(|d| bson::get(d, "batchSize"))
            .and_then(B::as_i64);
        self.cursor_reply(format!("{db}.{c}"), docs, batch, false)
    }

    /// `findAndModify`: the first match (by `sort`) updated, replaced or
    /// removed, and it — before or (`new`) after — in one transaction.
    fn find_and_modify(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let in_block = self
            .session
            .as_ref()
            .is_some_and(|s| s.status() != nucleus_sql::ext::TxnStatus::Idle);
        if !in_block {
            self.sql("BEGIN").map_err(|e| from_sql(&e))?;
        }
        let r = self.find_and_modify_in(&c, cmd);
        if !in_block {
            let end = if r.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let done = self.sql(end);
            if r.is_ok()
                && let Err(e) = done
            {
                return Err(from_sql(&e));
            }
        }
        r
    }

    fn find_and_modify_in(&mut self, c: &str, cmd: &Doc) -> Reply {
        let remove = bson::get(cmd, "remove").is_some_and(B::truthy);
        let new = bson::get(cmd, "new").is_some_and(B::truthy);
        let upsert = bson::get(cmd, "upsert").is_some_and(B::truthy);
        let fields = Self::json_part(bson::get(cmd, "fields"))?;
        let found = {
            let r = Request {
                op: Some(Op::Find),
                collection: c.to_string(),
                filter: filter_json(bson::get(cmd, "query"))?,
                sort: Self::json_part(bson::get(cmd, "sort"))?,
                limit: 1,
                ..Request::default()
            };
            let (rows, _) = self.reading(&r).map_err(|e| from_sql(&e))?;
            Self::docs_of(&rows)?.pop_front()
        };
        let by_id = |id: &B| bson::doc_json(&[("_id".into(), id.clone())], false);
        let mut last = vec![
            ("n".into(), B::Int32(0)),
            ("updatedExisting".into(), B::Bool(false)),
        ];
        let value = match (found, remove) {
            (Some(doc), true) => {
                let id = bson::get(&doc, "_id").cloned().unwrap_or(B::Null);
                self.reading(&Request {
                    op: Some(Op::Delete),
                    collection: c.to_string(),
                    filter: by_id(&id),
                    ..Request::default()
                })
                .map_err(|e| from_sql(&e))?;
                last = vec![("n".into(), B::Int32(1))];
                Some(doc)
            }
            (None, true) => None,
            (found, false) => {
                let Some(B::Doc(update)) = bson::get(cmd, "update") else {
                    return Err(bad_value("findAndModify takes update or remove"));
                };
                let target = match &found {
                    Some(doc) => B::Doc(vec![(
                        "_id".into(),
                        bson::get(doc, "_id").cloned().unwrap_or(B::Null),
                    )]),
                    None => bson::get(cmd, "query")
                        .cloned()
                        .unwrap_or(B::Doc(Vec::new())),
                };
                let u: Doc = vec![
                    ("q".into(), target),
                    ("u".into(), B::Doc(update.clone())),
                    ("upsert".into(), B::Bool(upsert && found.is_none())),
                ];
                let (_, _, upserted) = self.update_one(c, &u)?;
                let id = match (&found, &upserted) {
                    (Some(doc), _) => bson::get(doc, "_id").cloned(),
                    (None, Some(id)) => Some(id.clone()),
                    (None, None) => None,
                };
                if found.is_some() {
                    last = vec![
                        ("n".into(), B::Int32(1)),
                        ("updatedExisting".into(), B::Bool(true)),
                    ];
                } else if let Some(id) = &upserted {
                    last = vec![
                        ("n".into(), B::Int32(1)),
                        ("updatedExisting".into(), B::Bool(false)),
                        ("upserted".into(), id.clone()),
                    ];
                }
                match (new, id) {
                    (true, Some(id)) => {
                        let (rows, _) = self
                            .reading(&Request {
                                op: Some(Op::Find),
                                collection: c.to_string(),
                                filter: by_id(&id),
                                limit: 1,
                                ..Request::default()
                            })
                            .map_err(|e| from_sql(&e))?;
                        Self::docs_of(&rows)?.pop_front()
                    }
                    (false, _) => found,
                    (true, None) => None,
                }
            }
        };
        let value = match value {
            Some(doc) if !fields.is_empty() => {
                let sql = format!(
                    "SELECT json_project({}, {})",
                    lit(&bson::doc_json(&doc, false)),
                    lit(&fields)
                );
                match self.sql(&sql).map_err(|e| from_sql(&e))? {
                    Output::Rows { rows, .. } => B::Doc(document(
                        &rows[0]
                            .first()
                            .and_then(json_text)
                            .unwrap_or_else(|| "{}".into()),
                    )?),
                    Output::Command(_) => B::Null,
                }
            }
            Some(doc) => B::Doc(doc),
            None => B::Null,
        };
        ok(vec![
            ("lastErrorObject".into(), B::Doc(last)),
            ("value".into(), value),
        ])
    }

    /// `createIndexes`: path indexes (`CREATE INDEX … ON c (a.b)`), unique
    /// ones too; a key's direction does not matter (an index is read both
    /// ways).  Text, geospatial and hashed indexes are not made.
    fn create_indexes(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        let Some(B::Arr(indexes)) = bson::get(cmd, "indexes") else {
            return Err(bad_value("createIndexes takes indexes"));
        };
        let created = match self.sql(&format!("CREATE COLLECTION IF NOT EXISTS {}", ident(&c))) {
            Ok(Output::Command(tag)) => !tag.is_empty() && !tag.contains("SKIP"),
            Ok(_) => false,
            Err(e) => return Err(from_sql(&e)),
        };
        for ix in indexes {
            let Some(ix) = ix.as_doc() else {
                return Err(bad_value("an index is a document"));
            };
            let Some(B::Doc(key)) = bson::get(ix, "key") else {
                return Err(bad_value("an index needs a key"));
            };
            let mut paths = Vec::new();
            for (path, dir) in key {
                if !matches!(dir, B::Int32(_) | B::Int64(_) | B::Double(_)) {
                    return Err(Fail(
                        67,
                        "CannotCreateIndex",
                        format!("{path}: only ascending and descending keys"),
                    ));
                }
                if path == "_id" && key.len() == 1 {
                    continue;
                }
                let ok_path = path.split('.').all(|p| {
                    !p.is_empty()
                        && p.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
                        && !p.starts_with(|ch: char| ch.is_ascii_digit())
                });
                if !ok_path {
                    return Err(Fail(
                        67,
                        "CannotCreateIndex",
                        format!("{path}: a path of plain names expected"),
                    ));
                }
                paths.push(path.split('.').map(ident).collect::<Vec<_>>().join("."));
            }
            if paths.is_empty() {
                continue;
            }
            let name = bson::get(ix, "name")
                .and_then(B::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    key.iter()
                        .map(|(k, _)| k.replace('.', "_"))
                        .collect::<Vec<_>>()
                        .join("_")
                });
            let unique = if bson::get(ix, "unique").is_some_and(B::truthy) {
                "UNIQUE "
            } else {
                ""
            };
            let sql = format!(
                "CREATE {unique}INDEX IF NOT EXISTS {} ON {} ({})",
                ident(&format!("{c}_{name}")),
                ident(&c),
                paths.join(", ")
            );
            self.sql(&sql).map_err(|e| from_sql(&e))?;
        }
        ok(vec![(
            "createdCollectionAutomatically".into(),
            B::Bool(created),
        )])
    }

    fn list_indexes(&mut self, cmd: &Doc, db: &str) -> Reply {
        let c = Self::collection(cmd)?;
        let docs = VecDeque::from([vec![
            ("v".into(), B::Int32(2)),
            ("key".into(), B::Doc(vec![("_id".into(), B::Int32(1))])),
            ("name".into(), B::Str("_id_".into())),
        ]]);
        self.cursor_reply(format!("{db}.{c}"), docs, None, true)
    }

    fn drop_indexes(&mut self, cmd: &Doc) -> Reply {
        let c = Self::collection(cmd)?;
        match bson::get(cmd, "index") {
            Some(B::Str(name)) if name != "*" && name != "_id_" => {
                self.sql(&format!(
                    "DROP INDEX IF EXISTS {}",
                    ident(&format!("{c}_{name}"))
                ))
                .map_err(|e| from_sql(&e))?;
                ok(Vec::new())
            }
            _ => Err(bad_value("dropIndexes takes one index's name")),
        }
    }
}

fn write_error(index: usize, f: &Fail) -> B {
    B::Doc(vec![
        ("index".into(), B::Int32(index as i32)),
        ("code".into(), B::Int32(f.0)),
        ("errmsg".into(), B::Str(f.2.clone())),
    ])
}

/// The next batch: at most `n` documents and about 16 MiB.
fn take_batch(docs: &mut VecDeque<Doc>, n: usize) -> Vec<B> {
    let mut out = Vec::new();
    let mut bytes = 0;
    while out.len() < n {
        let Some(d) = docs.pop_front() else { break };
        let size = bson::write_doc(&d).len();
        if !out.is_empty() && bytes + size > MAX_BSON - 64 * 1024 {
            docs.push_front(d);
            break;
        }
        bytes += size;
        out.push(B::Doc(d));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_for_the_compiler() {
        let at = B::Date(1_700_000_000_000);
        let f = vec![
            ("name".into(), B::Regex("^a".into(), "i".into())),
            (
                "at".into(),
                B::Doc(vec![
                    ("$gte".into(), at.clone()),
                    ("$exists".into(), B::Bool(true)),
                ]),
            ),
        ];
        assert_eq!(
            bson::doc_json(&filter_doc(&f), true),
            r#"{"$and": [{"name": {"$regex": "^a", "$options": "i"}, "at": {"$exists": true}}, {"at.$date": {"$gte": "2023-11-14T22:13:20.000Z"}}]}"#
        );
        // Equality with a date: with its Extended JSON object, exact.
        let eq = vec![("at".into(), at)];
        assert_eq!(
            bson::doc_json(&filter_doc(&eq), true),
            r#"{"at": {"$eq": {"$date": "2023-11-14T22:13:20.000Z"}}}"#
        );
        assert_eq!(scram_user("n,,n=a=2Cb=3Dc,r=xyz").as_deref(), Some("a,b=c"));
    }
}
