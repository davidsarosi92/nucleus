//! A database in the process: in memory and in a directory, its sessions
//! over the in-process pipe, the C API's calls (PLAN.md §M14.23).

// The C API is called as C would: through raw pointers.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char};
use std::io::{Read, Write};
use std::ptr;

use nucleus_client::{Client, Config, Format, Value};
use nucleus_embedded::{Database, MEMORY, ffi};
use nucleus_testkit::slt::{Cell, Connection, SqlError, run_script};

fn client(db: &Database) -> Client {
    Client::over(Box::new(db.connect().unwrap()), Config::default()).unwrap()
}

fn one(c: &mut Client, sql: &str) -> Value {
    c.rows(sql, &[]).unwrap().remove(0).remove(0)
}

/// A fresh directory under the OS temp dir, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = nucleus_platform::temp_dir().join(format!(
            "nucleus-embedded-{name}-{}",
            nucleus_platform::process_id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn in_memory_sessions_share_one_database() {
    let db = Database::open(MEMORY, "").unwrap();
    let mut a = client(&db);
    let mut b = client(&db);
    a.execute("CREATE TABLE t (id int PRIMARY KEY, v vector(3))", &[])
        .unwrap();
    a.execute("INSERT INTO t VALUES (1, '[1,2,3]'), (2, '[0,0,1]')", &[])
        .unwrap();
    assert_eq!(one(&mut b, "SELECT count(*) FROM t"), Value::Int(2));
    assert_eq!(
        one(&mut b, "SELECT id FROM t ORDER BY v <-> '[0,0,1]' LIMIT 1"),
        Value::Integer(2)
    );
    // Another in-memory database is another database.
    let other = Database::open(MEMORY, "").unwrap();
    let mut c = client(&other);
    assert!(c.execute("SELECT * FROM t", &[]).is_err());
    a.close().unwrap();
    drop(b);
    db.close().unwrap();
}

#[test]
fn a_directory_keeps_its_data_and_is_shared_in_the_process() {
    let dir = TempDir::new("keeps");
    {
        let db = Database::open(dir.path(), "cache=16MB").unwrap();
        let mut c = client(&db);
        c.execute("CREATE TABLE t (id int PRIMARY KEY, body text)", &[])
            .unwrap();
        c.execute("INSERT INTO t VALUES (1, 'kept')", &[]).unwrap();
        // Opened again in the process: the same database, not a lock error.
        let again = Database::open(dir.path(), "").unwrap();
        let mut d = client(&again);
        assert_eq!(
            one(&mut d, "SELECT body FROM t"),
            Value::Text("kept".into())
        );
        drop(d);
        again.close().unwrap();
        // Still open through the first handle.
        assert_eq!(one(&mut c, "SELECT count(*) FROM t"), Value::Int(1));
        db.close().unwrap();
    }
    // Closed: the directory is free, and holds the table.
    let db = Database::open(dir.path(), "").unwrap();
    let mut c = client(&db);
    assert_eq!(
        one(&mut c, "SELECT body FROM t"),
        Value::Text("kept".into())
    );
}

#[test]
fn closing_the_database_ends_its_sessions() {
    let db = Database::open(MEMORY, "").unwrap();
    let mut c = client(&db);
    assert_eq!(one(&mut c, "SELECT 1"), Value::Integer(1));
    db.close().unwrap();
    assert!(c.execute("SELECT 1", &[]).is_err());
}

#[test]
fn a_large_result_streams_through_the_pipe() {
    let db = Database::open(MEMORY, "").unwrap();
    let mut c = client(&db);
    // Far more than the pipe holds: the session waits for the reader.
    let rows = c
        .rows(
            "SELECT repeat('x', 1000) FROM generate_series(1, 20000)",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 20000);
}

#[test]
fn options_are_checked() {
    assert!(Database::open(MEMORY, "cache=nonsense").is_err());
    assert!(Database::open(MEMORY, "colour=blue").is_err());
    assert!(Database::open(MEMORY, "sync").is_err());
    assert!(Database::open(MEMORY, "sync=full; wal=full, cache=8MB").is_ok());
    assert!(Database::open("", "").is_err());
}

/// Frames one message as the protocol does: kind, length, body.
fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![kind];
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(body);
    f
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[test]
fn the_c_api_opens_connects_and_carries_bytes() {
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let bad = CString::new("colour=blue").unwrap();
        let path = CString::new(MEMORY).unwrap();
        assert!(ffi::nce_open(path.as_ptr(), bad.as_ptr(), &mut err).is_null());
        assert!(CStr::from_ptr(err).to_str().unwrap().contains("colour"));
        ffi::nce_free_error(err);

        let db = ffi::nce_open(path.as_ptr(), ptr::null(), ptr::null_mut());
        assert!(!db.is_null());
        let c = ffi::nce_connect(db, ptr::null_mut());
        assert!(!c.is_null());
        // Nothing sent yet: the wait runs out.
        let mut buf = [0u8; 4096];
        assert_eq!(ffi::nce_recv(c, buf.as_mut_ptr(), buf.len(), 20), -1);
        // Hello: the version, user, database, password (none), client.
        let mut hello = nucleus_client::PROTOCOL_VERSION.to_le_bytes().to_vec();
        for s in ["nucleus", "nucleus", "", "test"] {
            put_str(&mut hello, s);
        }
        let h = frame(b'H', &hello);
        assert_eq!(ffi::nce_send(c, h.as_ptr(), h.len()), 0);
        let n = ffi::nce_recv(c, buf.as_mut_ptr(), buf.len(), -1);
        assert!(n > 0);
        // Welcome first: no login in the process.
        assert_eq!(buf[0], b'W');
        // Bye; the session ends, and so does what it sends.
        let bye = frame(b'X', &[]);
        assert_eq!(ffi::nce_send(c, bye.as_ptr(), bye.len()), 0);
        while ffi::nce_recv(c, buf.as_mut_ptr(), buf.len(), -1) > 0 {}
        ffi::nce_disconnect(c);
        assert_eq!(ffi::nce_close(db, ptr::null_mut()), 0);
        assert!(!CStr::from_ptr(ffi::nce_version()).to_bytes().is_empty());
    }
}

/// The slt connection over the in-process pipe (text results, as the
/// scripts compare text).
struct Ncp(Client);

impl Connection for Ncp {
    fn run(&mut self, sql: &str) -> Result<Option<Vec<Vec<Cell>>>, SqlError> {
        let mut replies = self.0.query(sql, &[], Format::Text).expect("connection");
        match replies.pop() {
            Some(Ok(r)) if r.has_rows => Ok(Some(
                r.rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|v| match v {
                                Value::Null => None,
                                Value::Text(s) => Some(s),
                                v => Some(v.to_string()),
                            })
                            .collect()
                    })
                    .collect(),
            )),
            Some(Ok(_)) | None => Ok(None),
            Some(Err(e)) => Err(SqlError {
                code: e.code,
                message: e.message,
            }),
        }
    }
}

#[test]
fn sql_scripts_in_the_process() {
    let mut failures = Vec::new();
    let scripts = nucleus_testkit::core_scripts();
    for (name, script) in scripts.iter().filter(|(n, _)| nucleus_testkit::picked(n)) {
        let db = Database::open(MEMORY, "").unwrap();
        let mut connect = || -> Box<dyn Connection> { Box::new(Ncp(client(&db))) };
        failures.extend(run_script(script, name, &mut connect));
    }
    assert!(scripts.len() >= 19);
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// A session stepped by its bytes ([`Database::exchange`], as the browser
/// drives it): what the client wrote is fed when it reads.
struct Stepped {
    exchange: nucleus_server::Exchange,
    sent: Vec<u8>,
    back: VecDeque<u8>,
}

impl Write for Stepped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sent.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for Stepped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.back.is_empty() && !self.sent.is_empty() {
            let sent = std::mem::take(&mut self.sent);
            self.back.extend(self.exchange.feed(&sent));
        }
        let n = buf.len().min(self.back.len());
        for (b, v) in buf.iter_mut().zip(self.back.drain(..n)) {
            *b = v;
        }
        Ok(n)
    }
}

fn stepped(db: &Database) -> Client {
    let s = Stepped {
        exchange: db.exchange().unwrap(),
        sent: Vec::new(),
        back: VecDeque::new(),
    };
    Client::over(Box::new(s), Config::default()).unwrap()
}

#[test]
fn sql_scripts_through_a_stepped_session() {
    let mut failures = Vec::new();
    let scripts = nucleus_testkit::core_scripts();
    for (name, script) in scripts.iter().filter(|(n, _)| nucleus_testkit::picked(n)) {
        let db = Database::open(MEMORY, "").unwrap();
        let mut connect = || -> Box<dyn Connection> { Box::new(Ncp(stepped(&db))) };
        failures.extend(run_script(script, name, &mut connect));
    }
    assert!(scripts.len() >= 19);
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn a_stepped_session_takes_copy_data_only_with_the_copy() {
    let db = Database::open(MEMORY, "").unwrap();
    let mut c = stepped(&db);
    c.query("CREATE TABLE t (a int)", &[], Format::Text)
        .unwrap();
    // The data comes after the COPY asked for it: too late — the COPY
    // fails (57014), the data that follows is dropped, the session goes on.
    let e = c
        .copy_in("COPY t FROM STDIN", &mut &b"1\n2\n"[..])
        .unwrap_err()
        .to_string();
    assert!(e.contains("no data came with it"), "{e}");
    assert_eq!(one(&mut c, "SELECT count(*) FROM t"), Value::Int(0));
    // A result fetched in batches comes whole, no batch waiting to be
    // asked for.
    let cur = c
        .cursor("SELECT i FROM generate_series(1, 2500) i", &[], 100)
        .unwrap();
    assert_eq!(cur.map(Result::unwrap).count(), 2500);
    assert_eq!(one(&mut c, "SELECT 7"), Value::Integer(7));
}
