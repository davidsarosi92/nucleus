//! A sample protocol adapter for nucleus (PLAN.md §M16.5 S6, D21):
//! newline-delimited JSON over TCP onto streams — the pattern a module
//! mapping an external protocol onto nucleus follows, built on the public
//! API alone (`nucleus_sql::ext::adapters`, `nucleus_server::FrontEnd`).
//!
//! One JSON object a line, each way:
//!
//! ```text
//! → {"login": "app", "password": "…"}
//! ← {"ok": "login"}
//! → {"append": "ticks", "entries": [{"symbol": "X", "price": 1.5}, …]}
//! ← {"ok": "append", "appended": 2}
//! → {"subscribe": "ticks", "after": 0}
//! ← {"seq": 1, "time": "…", "symbol": "X", "price": 1.5}
//! ← {"heartbeat": 1}
//! ← {"error": {"code": "22000", "message": "entries 1 to 4 … are gone …", "begins": 5}}
//! ```
//!
//! - `login` first: the server's users and passwords (and its login
//!   throttle and audit); refused, the connection ends.  The password
//!   travels as written: on a network, the server's TLS (a client that
//!   starts with a TLS handshake gets it) or a listener on `127.0.0.1`.
//! - `append` (`entries`, an array, or `entry`, one object) appends in one
//!   transaction: each object's fields by the stream's column names,
//!   converted to their types.
//! - `subscribe` turns the connection into the stream's entries after
//!   `after` (0: from the first), one a line in `seq` order, until the
//!   client goes away; `{"heartbeat": seq}` (the last `seq` sent) after
//!   ten seconds with none, so either side learns the other is gone.  A
//!   gap — entries after `after` the retention trimmed — is an error
//!   naming them and `begins` (where the stream now begins), and ends the
//!   connection: the client resubscribes from there if it accepts the gap.
//! - Every error is `{"error": {"code": SQLSTATE, "message": …}}`; after a
//!   refused `append` the connection goes on.
//!
//! ```no_run
//! # use std::sync::Arc;
//! let mut fronts = nucleus_server::FrontEnds::builtin();
//! fronts.add(Arc::new(nucleus_ndjson::Ndjson)).unwrap();
//! ```

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::Kv;
use nucleus_server::{Config, FrontEnd, Stream};
use nucleus_sql::ext::Session;
use nucleus_sql::ext::adapters::{Member, Subscription, append_json, members};

/// Where it listens unless told otherwise.
pub const DEFAULT_ADDR: &str = "127.0.0.1:20038";

/// The longest line read (a request), in bytes.
pub const MAX_LINE: u64 = 16 << 20;

/// How long a subscription waits for an entry before a heartbeat.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// How many entries a subscription reads at a time.
const BATCH: u32 = 1000;

/// The front end (`nucleus server --front-end ndjson [addr]`).
pub struct Ndjson;

impl FrontEnd for Ndjson {
    fn name(&self) -> &str {
        "ndjson"
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
        handle(stream, kv, config)
    }
}

/// Serves one client until it goes away; TLS when it starts with a
/// handshake and the server has it.
pub fn handle(mut stream: TcpStream, kv: Arc<dyn Kv>, config: &Config) -> io::Result<()> {
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
        return serve(secured.stream, kv, config, peer, true);
    }
    serve(Box::new(stream), kv, config, peer, false)
}

/// Serves one client over any byte stream (no TLS, no client address).
pub fn handle_stream(stream: Box<dyn Stream>, kv: Arc<dyn Kv>, config: &Config) -> io::Result<()> {
    serve(stream, kv, config, None, false)
}

/// A JSON string.
fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn error_line(code: &str, message: &str, extra: &str) -> String {
    format!(
        "{{\"error\": {{\"code\": {}, \"message\": {}{extra}}}}}",
        string(code),
        string(message)
    )
}

fn send(out: &mut impl Write, line: &str) -> io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

fn text<'a>(req: &'a [(String, Member)], key: &str) -> Option<&'a str> {
    req.iter().find_map(|(k, m)| match m {
        Member::Text(t) if k == key => Some(t.as_str()),
        _ => None,
    })
}

fn member<'a>(req: &'a [(String, Member)], key: &str) -> Option<&'a Member> {
    req.iter().find(|(k, _)| k == key).map(|(_, m)| m)
}

struct Conn<'a> {
    kv: Arc<dyn Kv>,
    config: &'a Config,
    peer: Option<IpAddr>,
    tls: bool,
}

impl Conn<'_> {
    fn context(&self, user: &str) -> nucleus_sql::ext::Context {
        nucleus_sql::ext::Context {
            client_addr: self.peer,
            front_end: "ndjson".into(),
            auth_method: nucleus_server::password_method(self.config, &*self.kv, user).into(),
            tls: self.tls,
        }
    }

    /// A session for `user` if `password` is theirs; the refusal's line.
    fn login(&self, user: &str, password: &str) -> Result<Session, String> {
        let (config, kv) = (self.config, &self.kv);
        if let Some(why) = config.throttle.locked(&**kv, user) {
            return Err(error_line("28000", &why, ""));
        }
        let accepted = if nucleus_server::directory_user(config, &**kv, user) {
            nucleus_server::directory_accepts(config, user, password).unwrap_or(false)
        } else {
            config.accepts(&**kv, user, password)
        };
        if !accepted {
            config.throttle.failed(&**kv, user, self.peer);
            let message = format!("password authentication failed for user \"{user}\"");
            let _ = nucleus_sql::ext::audit_refused_login(
                kv,
                user,
                self.context(user),
                "28P01",
                &message,
            );
            return Err(error_line("28P01", &message, ""));
        }
        config.throttle.succeeded(user);
        let mut session = Session::with_registry(Arc::clone(kv), Arc::clone(&config.registry));
        session
            .login(user)
            .and_then(|()| nucleus_server::room_for(config, kv, &session))
            .and_then(|()| session.admit(self.context(user)))
            .map_err(|e| error_line(e.code, &e.message, ""))?;
        Ok(session)
    }
}

fn serve(
    stream: Box<dyn Stream>,
    kv: Arc<dyn Kv>,
    config: &Config,
    peer: Option<IpAddr>,
    tls: bool,
) -> io::Result<()> {
    let (r, w) = nucleus_server::transport::split(stream);
    let mut input = BufReader::with_capacity(64 << 10, r);
    let mut out = BufWriter::with_capacity(64 << 10, w);
    let conn = Conn {
        kv,
        config,
        peer,
        tls,
    };
    let mut session: Option<Session> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut input).take(MAX_LINE + 1).read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if n as u64 > MAX_LINE {
            let too_long = format!("a line is at most {MAX_LINE} bytes");
            return send(&mut out, &error_line("54000", &too_long, ""));
        }
        if line.trim().is_empty() {
            continue;
        }
        let req = match members(line.trim()) {
            Ok(req) => req,
            Err(e) => {
                send(&mut out, &error_line(e.code, &e.message, ""))?;
                continue;
            }
        };
        let Some(s) = session.as_mut() else {
            // Nothing before a login.
            let (Some(user), Some(password)) = (text(&req, "login"), text(&req, "password")) else {
                return send(
                    &mut out,
                    &error_line(
                        "28000",
                        "the first line is {\"login\": …, \"password\": …}",
                        "",
                    ),
                );
            };
            match conn.login(user, password) {
                Ok(s) => {
                    session = Some(s);
                    send(&mut out, "{\"ok\": \"login\"}")?;
                    continue;
                }
                Err(refused) => return send(&mut out, &refused),
            }
        };
        if let Some(name) = text(&req, "append") {
            let entries = match (member(&req, "entries"), member(&req, "entry")) {
                (Some(Member::Json(j)), None) if j.starts_with('[') => j,
                (None, Some(Member::Json(j))) if j.starts_with('{') => j,
                _ => {
                    send(
                        &mut out,
                        &error_line(
                            "22023",
                            "append takes \"entries\" (an array of objects) or \"entry\" (an object)",
                            "",
                        ),
                    )?;
                    continue;
                }
            };
            let reply = match append_json(s, name, entries) {
                Ok(n) => format!("{{\"ok\": \"append\", \"appended\": {n}}}"),
                Err(e) => error_line(e.code, &e.message, ""),
            };
            send(&mut out, &reply)?;
        } else if let Some(name) = text(&req, "subscribe") {
            let after = match member(&req, "after") {
                None => 0,
                Some(Member::Int(n)) if *n >= 0 => *n,
                Some(_) => {
                    send(
                        &mut out,
                        &error_line("22023", "\"after\" is a seq (0: from the first)", ""),
                    )?;
                    continue;
                }
            };
            return subscribe(s, &mut out, Subscription::new(name, after));
        } else {
            send(
                &mut out,
                &error_line(
                    "22023",
                    "a request is {\"append\": …} or {\"subscribe\": …}",
                    "",
                ),
            )?;
        }
    }
}

/// Sends `sub`'s entries until the client goes away (a write fails) or a
/// gap or an error ends it.
fn subscribe(s: &mut Session, out: &mut impl Write, mut sub: Subscription) -> io::Result<()> {
    loop {
        match sub.next(s, BATCH, HEARTBEAT) {
            Ok(entries) if entries.is_empty() => {
                send(out, &format!("{{\"heartbeat\": {}}}", sub.after()))?;
            }
            Ok(entries) => {
                for e in &entries {
                    out.write_all(e.as_bytes())?;
                    out.write_all(b"\n")?;
                }
                out.flush()?;
            }
            Err(e) => {
                let begins = match e.code {
                    "22000" => sub
                        .start(s)
                        .map(|n| format!(", \"begins\": {n}"))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                return send(out, &error_line(e.code, &e.message, &begins));
            }
        }
    }
}
