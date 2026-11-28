//! The sample adapter over a socket (PLAN.md §M16.5 S6): a login, appends
//! converted to the stream's types, a subscription that replays from a
//! `seq` and goes on live, a gap the retention made named with where the
//! stream begins, and refusals that leave the connection usable.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Database, Kv, Options};
use nucleus_ndjson::Ndjson;
use nucleus_platform::sim::SimPlatform;
use nucleus_server::{Config, serve_front_end};
use nucleus_sql::ext::Session;

fn start() -> (SocketAddr, Arc<dyn Kv>, Arc<SimPlatform>) {
    let platform = Arc::new(SimPlatform::new(1));
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(platform.clone(), Path::new("/db"), Options::default()).unwrap(),
    );
    let mut s = Session::new(db.clone());
    s.execute(
        "CREATE USER admin SUPERUSER PASSWORD 'root-pw'; CREATE USER app PASSWORD 'secret'; \
         CREATE STREAM ticks (symbol TEXT NOT NULL, price NUMERIC(10,2), at TIMESTAMPTZ, tags JSON) \
           WITH (max_entries = 3); \
         CREATE STREAM brief (n INT) WITH (retention = '1 second'); \
         CREATE TABLE plain (a INT); \
         GRANT SELECT, INSERT ON ticks, brief TO app",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let kv = db.clone();
    std::thread::spawn(move || serve_front_end(Arc::new(Ndjson), listener, kv, Config::default()));
    (addr, db, platform)
}

struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let w = TcpStream::connect(addr).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        Client { w, r }
    }

    fn login(addr: SocketAddr) -> Client {
        let mut c = Client::connect(addr);
        assert_eq!(
            c.ask(r#"{"login": "app", "password": "secret"}"#),
            r#"{"ok": "login"}"#
        );
        c
    }

    fn send(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).unwrap();
        self.w.write_all(b"\n").unwrap();
    }

    /// The next line, or "" at the end.
    fn line(&mut self) -> String {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        l.trim_end().to_string()
    }

    fn ask(&mut self, line: &str) -> String {
        self.send(line);
        self.line()
    }
}

fn code(line: &str) -> &str {
    let at = line.find("\"code\": \"").expect(line) + 9;
    &line[at..at + 5]
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn appends_then_replays_and_follows_live() {
    let (addr, _db, _) = start();
    let mut c = Client::login(addr);
    assert_eq!(
        c.ask(
            r#"{"append": "ticks", "entries": [{"symbol": "AB", "price": 1.5, "at": "2026-01-02T03:04:05Z", "tags": {"x": [1]}, "unknown": 7}, {"symbol": "CD"}]}"#
        ),
        r#"{"ok": "append", "appended": 2}"#
    );
    assert_eq!(
        c.ask(r#"{"append": "ticks", "entry": {"symbol": "EF", "price": "2.25"}}"#),
        r#"{"ok": "append", "appended": 1}"#
    );
    // Replayed from after 1, typed as the stream's columns.
    let mut sub = Client::login(addr);
    sub.send(r#"{"subscribe": "ticks", "after": 1}"#);
    let second = sub.line();
    for part in [
        r#""seq": 2"#,
        r#""at": null"#,
        r#""price": null"#,
        r#""symbol": "CD""#,
        r#""time": "20"#,
    ] {
        assert!(second.contains(part), "{second}");
    }
    let third = sub.line();
    assert!(
        third.contains(r#""seq": 3"#) && third.contains(r#""price": 2.25"#),
        "{third}"
    );
    // Then live: appended on another connection while it waits.
    assert_eq!(
        c.ask(r#"{"append": "ticks", "entry": {"symbol": "GH"}}"#),
        r#"{"ok": "append", "appended": 1}"#
    );
    let fourth = sub.line();
    assert!(
        fourth.contains(r#""seq": 4"#) && fourth.contains(r#""symbol": "GH""#),
        "{fourth}"
    );
    // From the first: a JSON column stays JSON, the timestamp its text.
    let mut all = Client::login(addr);
    all.send(r#"{"subscribe": "ticks"}"#);
    let first = all.line();
    assert!(
        first.contains(r#""at": "2026-01-02T03:04:05+00:00""#)
            && first.contains(r#""tags": {"x": [1]}"#)
            && first.contains(r#""price": 1.5"#)
            && !first.contains("unknown"),
        "{first}"
    );
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_gap_is_named_with_where_the_stream_begins() {
    let (addr, db, _) = start();
    let mut c = Client::login(addr);
    for i in 0..6 {
        let line = format!(r#"{{"append": "ticks", "entry": {{"symbol": "S{i}"}}}}"#);
        assert_eq!(c.ask(&line), r#"{"ok": "append", "appended": 1}"#);
    }
    // max_entries = 3: 1 to 3 trimmed.
    assert_eq!(
        nucleus_sql::ext::adapters::trim_all(&db, i64::MAX / 2).unwrap(),
        3
    );
    let mut from_one = Client::login(addr);
    let gap = from_one.ask(r#"{"subscribe": "ticks", "after": 1}"#);
    assert_eq!(code(&gap), "22000", "{gap}");
    assert!(
        gap.contains("entries 2 to 3 of stream \\\"ticks\\\" are gone")
            && gap.ends_with(r#", "begins": 4}}"#),
        "{gap}"
    );
    assert_eq!(from_one.line(), "", "the connection ends");
    // From where it begins, or past it: no gap.
    let mut from_three = Client::login(addr);
    from_three.send(r#"{"subscribe": "ticks", "after": 3}"#);
    assert!(from_three.line().contains(r#""seq": 4"#));
    let mut from_five = Client::login(addr);
    from_five.send(r#"{"subscribe": "ticks", "after": 5}"#);
    assert!(from_five.line().contains(r#""seq": 6"#));
    // All of a stream trimmed: it begins after its newest.
    assert_eq!(
        c.ask(r#"{"append": "brief", "entries": [{"n": 1}, {"n": 2}]}"#),
        r#"{"ok": "append", "appended": 2}"#
    );
    assert_eq!(
        nucleus_sql::ext::adapters::trim_all(&db, i64::MAX / 2).unwrap(),
        2
    );
    let mut all_gone = Client::login(addr);
    let gap = all_gone.ask(r#"{"subscribe": "brief"}"#);
    assert!(
        gap.contains("entries 1 to 2 of stream \\\"brief\\\" are gone")
            && gap.ends_with(r#", "begins": 3}}"#),
        "{gap}"
    );
    let mut caught_up = Client::login(addr);
    caught_up.send(r#"{"subscribe": "brief", "after": 2}"#);
    c.send(r#"{"append": "brief", "entry": {"n": 3}}"#);
    assert!(caught_up.line().contains(r#""n": 3, "seq": 3"#));
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn refusals() {
    let (addr, _db, _) = start();
    // Nothing before a login; a wrong password ends the connection.
    let mut c = Client::connect(addr);
    assert_eq!(code(&c.ask(r#"{"append": "ticks", "entry": {}}"#)), "28000");
    assert_eq!(c.line(), "");
    let mut c = Client::connect(addr);
    assert_eq!(
        code(&c.ask(r#"{"login": "app", "password": "wrong"}"#)),
        "28P01"
    );
    assert_eq!(c.line(), "");
    // Refused requests leave the connection usable.
    let mut c = Client::login(addr);
    for (request, want) in [
        ("not json", "22P02"),
        ("[1, 2]", "22023"),
        (r#"{"what": 1}"#, "22023"),
        (
            r#"{"append": "ticks", "entries": {"symbol": "A"}}"#,
            "22023",
        ),
        (r#"{"append": "missing", "entry": {"a": 1}}"#, "42P01"),
        (r#"{"append": "plain", "entry": {"a": 1}}"#, "42809"),
        (r#"{"append": "ticks", "entry": {"price": 1}}"#, "23502"),
        (
            r#"{"append": "ticks", "entry": {"symbol": "A", "price": "x"}}"#,
            "22P02",
        ),
        (
            r#"{"append": "ticks", "entry": {"symbol": "A", "price": [1]}}"#,
            "2203F",
        ),
        (
            r#"{"append": "ticks", "entries": [{"symbol": "A"}, 1]}"#,
            "22023",
        ),
        (
            r#"{"append": "ticks", "entries": [{"symbol": "A"}, {"price": 1}]}"#,
            "23502",
        ),
        (r#"{"subscribe": "ticks", "after": -1}"#, "22023"),
    ] {
        let got = c.ask(request);
        assert_eq!(code(&got), want, "{request}: {got}");
    }
    assert_eq!(
        c.ask(r#"{"append": "ticks", "entries": [{"symbol": "A"}, {"symbol": "B"}]}"#),
        r#"{"ok": "append", "appended": 2}"#
    );
    // A privilege it lacks: no DELETE needed, but no other stream either.
    let mut s = Session::new(_db.clone());
    s.execute("CREATE STREAM private (x INT)").unwrap();
    assert_eq!(
        code(&c.ask(r#"{"append": "private", "entry": {"x": 1}}"#)),
        "42501"
    );
    // A line past the longest: refused, the connection ends.
    let mut long = Client::login(addr);
    let line = vec![b' '; nucleus_ndjson::MAX_LINE as usize + 1];
    long.w.write_all(&line).unwrap();
    assert_eq!(code(&long.line()), "54000");
    assert_eq!(long.line(), "");
    // A failed append appended nothing: the stream holds A and B only.
    c.send(r#"{"subscribe": "ticks"}"#);
    assert!(c.line().contains(r#""symbol": "A""#));
    assert!(c.line().contains(r#""symbol": "B""#));
}
