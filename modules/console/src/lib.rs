//! The web console (PLAN.md §M14.17 E3): a front end that speaks HTTP to
//! a browser — a page to browse the tables, run SQL, see plans drawn and
//! the linter's findings — served by the server itself
//! (`nucleus server --console`, or `--front-end console`).
//!
//! The page (`web/`) is compiled in.  It logs in with a user and password,
//! checked as the other front ends check them (the server's
//! authenticator, the directory, the throttle, refusals audited); the
//! session it gets is a database session like any client's, kept here
//! under a random token in a cookie (`HttpOnly`, `SameSite=Strict`) and
//! dropped after [`IDLE`] unused.  A statement's rows stream into the
//! reply until [`MAX_ROWS`], where the statement stops.  Requests that
//! change anything are `POST`s carrying a header a cross-site form cannot
//! send, from the page's own origin.  Over TLS when the server has a
//! certificate; the edition refuses to listen beyond loopback without it.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nucleus_kv::Kv;
use nucleus_server::transport::Stream;
use nucleus_server::{Config, FrontEnd};
use nucleus_sql::ext::{Context, OutColumn, Row, Session, Sink};
use serde_json::{Value as Json, json};

/// Where the console listens unless told otherwise.
pub const DEFAULT_ADDR: &str = "127.0.0.1:20036";

/// How long a session may sit unused before it is dropped.
pub const IDLE: Duration = Duration::from_secs(30 * 60);

/// The most rows a statement's reply carries (the page may ask for fewer).
pub const MAX_ROWS: usize = 10_000;

/// The most sessions kept at once; the least recently used gives way.
pub const MAX_SESSIONS: usize = 64;

/// A request's head and body, at most.
const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 1 << 20;

/// How long a connection may wait for its next request.
const KEEP_ALIVE: Duration = Duration::from_secs(60);

const COOKIE: &str = "nucleus_console";

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");

/// The console: the front end, and the sessions its page holds.
#[derive(Default)]
pub struct Console {
    sessions: Mutex<HashMap<String, Arc<Mutex<Held>>>>,
}

/// A logged-in page's database session.
struct Held {
    session: Session,
    user: String,
    database: String,
    last: Instant,
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Session>();
};

impl FrontEnd for Console {
    fn name(&self) -> &str {
        "console"
    }

    fn default_addr(&self) -> String {
        DEFAULT_ADDR.into()
    }

    fn handle(
        &self,
        stream: TcpStream,
        kv: Arc<dyn Kv>,
        config: &Config,
        _id: u32,
    ) -> io::Result<()> {
        let peer = stream.peer_addr().ok().map(|a| a.ip());
        stream.set_read_timeout(Some(KEEP_ALIVE))?;
        let (stream, tls): (Box<dyn Stream>, bool) = match &config.tls {
            Some(t) => (t.accept(stream)?.stream, true),
            None => (Box::new(stream), false),
        };
        let (r, mut w) = nucleus_server::transport::split(stream);
        let mut r = BufReader::new(r);
        let ctx = Ctx {
            console: self,
            kv: &kv,
            config,
            peer,
            tls,
        };
        loop {
            let req = match read_request(&mut r) {
                Ok(Some(req)) => req,
                Ok(None) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    respond(&mut w, &Reply::text(400, &e.to_string()), false)?;
                    return Ok(());
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let keep = req.keep_alive();
            let reply = ctx.route(&req);
            respond(&mut w, &reply, keep)?;
            if !keep {
                return Ok(());
            }
        }
    }
}

/// What a request is answered in.
struct Ctx<'a> {
    console: &'a Console,
    kv: &'a Arc<dyn Kv>,
    config: &'a Config,
    peer: Option<IpAddr>,
    tls: bool,
}

impl Ctx<'_> {
    fn route(&self, req: &Request) -> Reply {
        match (req.method.as_str(), req.path()) {
            ("GET", "/") => Reply::asset("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.js") => Reply::asset("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/app.css") => Reply::asset("text/css; charset=utf-8", APP_CSS),
            ("GET", "/api/me") => match self.held(req) {
                Some(h) => {
                    let h = lock(&h);
                    Reply::json(200, &json!({ "user": h.user, "database": h.database }))
                }
                None => Reply::error(401, "28000", "not logged in"),
            },
            ("POST", path @ ("/api/login" | "/api/logout" | "/api/query")) => {
                if let Some(refused) = self.cross_site(req) {
                    return refused;
                }
                let body: Json = match serde_json::from_slice(&req.body) {
                    Ok(b) => b,
                    Err(e) => {
                        return Reply::error(400, "08P01", &format!("the request's JSON: {e}"));
                    }
                };
                match path {
                    "/api/login" => self.login(&body),
                    "/api/logout" => {
                        if let Some(token) = req.cookie(COOKIE) {
                            self.console.sessions().remove(token);
                        }
                        Reply::json(200, &json!({})).cookie(&format!(
                            "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
                            if self.tls { "; Secure" } else { "" }
                        ))
                    }
                    _ => match self.held(req) {
                        Some(h) => query(&mut lock(&h), &body),
                        None => Reply::error(401, "28000", "not logged in"),
                    },
                }
            }
            ("GET" | "POST", _) => Reply::text(404, "not found"),
            _ => Reply::text(405, "method not allowed"),
        }
    }

    /// A request a page of another origin could have made: refused.  A
    /// form cannot set a header, and a script of another origin may not
    /// without the server's leave (CORS), which it never gives.
    fn cross_site(&self, req: &Request) -> Option<Reply> {
        if req.header("x-nucleus-console") != Some("1") {
            return Some(Reply::error(
                403,
                "42501",
                "a console request needs X-Nucleus-Console: 1",
            ));
        }
        if let (Some(origin), Some(host)) = (req.header("origin"), req.header("host")) {
            let own = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                .is_some_and(|o| o.eq_ignore_ascii_case(host));
            if !own {
                return Some(Reply::error(403, "42501", "a request from another origin"));
            }
        }
        None
    }

    /// The session a request's cookie names, if it is still kept.
    fn held(&self, req: &Request) -> Option<Arc<Mutex<Held>>> {
        let token = req.cookie(COOKIE)?;
        let mut sessions = self.console.sessions();
        sessions.retain(|_, h| h.try_lock().map_or(true, |h| h.last.elapsed() < IDLE));
        let h = Arc::clone(sessions.get(token)?);
        drop(sessions);
        lock(&h).last = Instant::now();
        Some(h)
    }

    fn login(&self, body: &Json) -> Reply {
        let field = |k: &str| body.get(k).and_then(Json::as_str).unwrap_or_default();
        let (user, password, database) = (field("user"), field("password"), field("database"));
        let (kv, config) = (self.kv, self.config);
        if let Some(why) = config.throttle.locked(&**kv, user) {
            return Reply::error(403, "28000", &why);
        }
        let loopback = self.peer.is_some_and(|p| p.is_loopback());
        let directory = nucleus_server::directory_user(config, &**kv, user);
        let proved = if !config.auth.wants_password() {
            true
        } else if directory {
            // The directory checks the password itself: only where it
            // cannot have been read on the way.
            (self.tls || loopback)
                && nucleus_server::directory_accepts(config, user, password).unwrap_or(false)
        } else {
            config.accepts(&**kv, user, password)
        };
        let method = if !config.auth.wants_password() {
            "trust"
        } else {
            nucleus_server::password_method(config, &**kv, user)
        };
        let context = Context {
            client_addr: self.peer,
            front_end: "console".into(),
            auth_method: method.into(),
            tls: self.tls,
        };
        if !proved {
            config.throttle.failed(&**kv, user, self.peer);
            let message = format!("password authentication failed for user \"{user}\"");
            let _ = nucleus_sql::ext::audit_refused_login(kv, user, context, "28P01", &message);
            return Reply::error(401, "28P01", &message);
        }
        let data = if database.is_empty() {
            Arc::clone(kv)
        } else {
            match nucleus_server::branch_of(kv, database) {
                Ok(d) => d,
                Err(e) => return Reply::sql_error(401, &e),
            }
        };
        let mut session = Session::with_registry(data, Arc::clone(&config.registry));
        let admitted = session
            .login(user)
            .and_then(|()| nucleus_server::room_for(config, kv, &session))
            .and_then(|()| session.admit(context));
        if let Err(e) = admitted {
            return Reply::sql_error(401, &e);
        }
        let token = {
            let mut b = [0u8; 32];
            kv.random_bytes(&mut b);
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        };
        let database = if database.is_empty() {
            nucleus_server::DATABASE_NAME.to_string()
        } else {
            database.to_string()
        };
        let held = Held {
            session,
            user: user.to_string(),
            database: database.clone(),
            last: Instant::now(),
        };
        {
            let mut sessions = self.console.sessions();
            while sessions.len() >= MAX_SESSIONS {
                let oldest = sessions
                    .iter()
                    .min_by_key(|(_, h)| h.try_lock().map_or(Instant::now(), |h| h.last))
                    .map(|(t, _)| t.clone());
                match oldest {
                    Some(t) => sessions.remove(&t),
                    None => break,
                };
            }
            sessions.insert(token.clone(), Arc::new(Mutex::new(held)));
        }
        Reply::json(
            200,
            &json!({ "user": user, "database": database, "version": env!("CARGO_PKG_VERSION") }),
        )
        .cookie(&format!(
            "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{}",
            if self.tls { "; Secure" } else { "" }
        ))
    }
}

impl Console {
    /// A console with no sessions yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Mutex<Held>>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn lock(h: &Mutex<Held>) -> MutexGuard<'_, Held> {
    h.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `body.sql`'s statements in order, each reply's rows up to
/// `body.limit` (at most [`MAX_ROWS`]); the first error ends the run and is
/// reported beside the results before it.
fn query(held: &mut Held, body: &Json) -> Reply {
    let sql = body.get("sql").and_then(Json::as_str).unwrap_or_default();
    let limit = body
        .get("limit")
        .and_then(Json::as_u64)
        .map_or(1000, |l| l as usize)
        .clamp(1, MAX_ROWS);
    let session = &mut held.session;
    let mut results = Vec::new();
    let error = match session.parse(sql) {
        Err(e) => Some(e),
        Ok(parsed) => {
            let mut error = None;
            for (stmt, text) in parsed.iter() {
                session.note_text(text);
                let mut sink = Collect {
                    columns: None,
                    rows: Vec::new(),
                    limit,
                    truncated: false,
                };
                let started = Instant::now();
                match session.run_into(stmt, &[], &mut sink) {
                    Ok(tag) => results.push(json!({
                        "command": tag,
                        "columns": sink.columns.as_ref().map(|c| c
                            .iter()
                            .map(|c| json!({ "name": c.name, "type": c.ty.to_string() }))
                            .collect::<Vec<_>>()),
                        "rows": sink.rows,
                        "truncated": sink.truncated,
                        "ms": started.elapsed().as_secs_f64() * 1e3,
                    })),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            error
        }
    };
    let status = match session.status() {
        nucleus_sql::ext::TxnStatus::Idle => "idle",
        nucleus_sql::ext::TxnStatus::InBlock => "in transaction",
        nucleus_sql::ext::TxnStatus::Failed => "failed transaction",
    };
    let mut out = json!({ "results": results, "transaction": status });
    if let Some(e) = error {
        out["error"] = error_json(&e);
    }
    Reply::json(200, &out)
}

/// A statement's reply, kept: rows as text (`NULL` as `null`), up to a
/// limit, where the statement stops.
struct Collect {
    columns: Option<Vec<OutColumn>>,
    rows: Vec<Vec<Option<String>>>,
    limit: usize,
    truncated: bool,
}

impl Sink for Collect {
    fn columns(&mut self, columns: &[OutColumn]) -> nucleus_sql::ext::Result<()> {
        self.columns = Some(columns.to_vec());
        Ok(())
    }

    fn row(&mut self, row: Row) -> nucleus_sql::ext::Result<ControlFlow<()>> {
        if self.rows.len() == self.limit {
            self.truncated = true;
            return Ok(ControlFlow::Break(()));
        }
        self.rows.push(
            row.iter()
                .map(|v| (!v.is_null()).then(|| v.render()))
                .collect(),
        );
        Ok(ControlFlow::Continue(()))
    }
}

fn error_json(e: &nucleus_sql::ext::Error) -> Json {
    json!({ "code": e.code, "message": e.message, "hint": e.hint })
}

/// An HTTP request.
struct Request {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, v)| v.split(';'))
            .filter_map(|c| c.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    fn keep_alive(&self) -> bool {
        let connection = self.header("connection").map(str::to_ascii_lowercase);
        match self.version.as_str() {
            "HTTP/1.1" => connection.as_deref() != Some("close"),
            _ => connection.as_deref() == Some("keep-alive"),
        }
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// The next request on a connection; `None` at its end.
fn read_request(r: &mut impl BufRead) -> io::Result<Option<Request>> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let n = r
            .take((MAX_HEAD - head.len()) as u64 + 1)
            .read_until(b'\n', &mut head)?;
        if n == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if head.len() > MAX_HEAD {
            return Err(bad("the request's head is too long"));
        }
        let line = &head[before..];
        // A blank line ends the head (leading ones are skipped, RFC 9112 2.2).
        if line == b"\r\n" || line == b"\n" {
            if before == 0 {
                head.clear();
                continue;
            }
            break;
        }
    }
    let head = std::str::from_utf8(&head).map_err(|_| bad("the request's head is not UTF-8"))?;
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(bad("a malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad("HTTP/1.x only"));
    }
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| bad("a malformed header"))?;
        if k.is_empty() || k.contains(char::is_whitespace) {
            return Err(bad("a malformed header"));
        }
        headers.push((k.to_string(), v.trim().to_string()));
    }
    let mut req = Request {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        body: Vec::new(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err(bad("chunked bodies are not taken: send Content-Length"));
    }
    let length = match req.header("content-length") {
        Some(l) => l
            .parse::<usize>()
            .map_err(|_| bad("a malformed Content-Length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(bad("the request's body is too large"));
    }
    req.body = vec![0; length];
    r.read_exact(&mut req.body)?;
    Ok(Some(req))
}

/// A response.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cookie: Option<String>,
    api: bool,
}

impl Reply {
    fn asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.as_bytes().to_vec(),
            cookie: None,
            api: false,
        }
    }

    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            cookie: None,
            api: true,
        }
    }

    fn json(status: u16, body: &Json) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
            cookie: None,
            api: true,
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(
            status,
            &json!({ "error": { "code": code, "message": message } }),
        )
    }

    fn sql_error(status: u16, e: &nucleus_sql::ext::Error) -> Self {
        Self::json(status, &json!({ "error": error_json(e) }))
    }

    fn cookie(mut self, cookie: &str) -> Self {
        self.cookie = Some(cookie.to_string());
        self
    }
}

fn respond(w: &mut impl Write, reply: &Reply, keep: bool) -> io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; img-src 'self' data:; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\
         Cache-Control: {}\r\nConnection: {}\r\n",
        reply.status,
        reply.content_type,
        reply.body.len(),
        if reply.api { "no-store" } else { "no-cache" },
        if keep { "keep-alive" } else { "close" },
    );
    if let Some(c) = &reply.cookie {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&reply.body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> io::Result<Option<Request>> {
        read_request(&mut BufReader::new(text.as_bytes()))
    }

    #[test]
    fn requests_are_read() {
        let r = parse(
            "POST /api/query?x=1 HTTP/1.1\r\nHost: localhost:20036\r\nCookie: a=1; nucleus_console=abc\r\n\
             Content-Length: 2\r\n\r\n{}",
        )
        .unwrap()
        .unwrap();
        assert_eq!((r.method.as_str(), r.path()), ("POST", "/api/query"));
        assert_eq!(r.cookie(COOKIE), Some("abc"));
        assert_eq!(r.header("HOST"), Some("localhost:20036"));
        assert_eq!(r.body, b"{}");
        assert!(r.keep_alive());
        assert!(parse("").unwrap().is_none());
    }

    #[test]
    fn malformed_requests_are_refused() {
        for text in [
            "GET /\r\n\r\n",
            "GET / HTTP/2\r\n\r\n",
            "GET / HTTP/1.1\r\nno colon\r\n\r\n",
            "GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            let e = parse(text).map(|_| ()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{text:?}");
        }
        let long = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(parse(&long).is_err());
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(parse(&big).is_err());
    }
}
