//! Where a read by key spends its time (PLAN.md §M14.33 R1): the suite's
//! `SELECT s FROM b WHERE id = <n>` on a table of `PHASES_ROWS` rows
//! (`bench/run.py`'s), in the process, phase by phase — and the
//! operating system's round trip on the two ways a client reaches a
//! server on one machine, loopback TCP (nucleus's) and a Unix socket
//! (`psql`'s default).  Not run by default:
//!
//! ```sh
//! PHASES_ROWS=1000000 cargo test --release -p nucleus-sql --test statement_phases \
//!     -- --ignored --nocapture
//! ```
//!
//! Without `PHASES_ROWS` (the CI's run of ignored tests) a table of
//! 20,000 rows.
//!
//! Each figure is µs a statement, the median of `ROUNDS` rounds of
//! `READS` statements, keys spread over the table; each is printed as an
//! `@metric` line too, for `bench/method.py run` (PLAN.md §M14.31).

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Instant;

use nucleus_kv::{Database, Kv, Options};
use nucleus_sql::{Output, Session, Value};

const READS: usize = 1000;
const ROUNDS: usize = 7;

fn rows() -> u64 {
    std::env::var("PHASES_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

/// The median over `ROUNDS` of `f`'s µs a call, `READS` calls a round
/// (the call's argument: the read's number in the round).
fn per_call(mut f: impl FnMut(usize)) -> f64 {
    let mut rounds: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let t = Instant::now();
            for i in 0..READS {
                f(i);
            }
            t.elapsed().as_secs_f64() * 1e6 / READS as f64
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    rounds[ROUNDS / 2]
}

fn report(what: &str, us: f64) {
    println!("{what}: {us:.2} µs");
    println!(
        "@metric {{\"name\": \"{what}\", \"values\": [{us}], \"unit\": \"µs\", \"system\": \"nucleus\", \"phase\": \"warm\"}}"
    );
}

/// `READS` keys spread over the table, as `bench/run.py`'s.
fn key(i: usize, n: u64) -> u64 {
    let step = (n / READS as u64).max(1);
    1 + (i as u64 * step) % n
}

fn round_trips_tcp() -> f64 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_nodelay(true).unwrap();
        echo(&mut s);
    });
    let mut c = std::net::TcpStream::connect(addr).unwrap();
    c.set_nodelay(true).unwrap();
    let us = per_call(|_| ping(&mut c));
    drop(c);
    server.join().unwrap();
    us
}

fn round_trips_unix() -> f64 {
    let dir = std::env::temp_dir().join(format!("nucleus-phases-{}", std::process::id()));
    let _ = std::fs::remove_file(&dir);
    let listener = std::os::unix::net::UnixListener::bind(&dir).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        echo(&mut s);
    });
    let mut c = std::os::unix::net::UnixStream::connect(&dir).unwrap();
    let us = per_call(|_| ping(&mut c));
    drop(c);
    server.join().unwrap();
    let _ = std::fs::remove_file(&dir);
    us
}

/// A request the size of the suite's statement, its answer a row's.
const REQUEST: usize = 64;
const ANSWER: usize = 96;

fn echo(s: &mut (impl Read + Write)) {
    let mut buf = [0u8; REQUEST];
    while s.read_exact(&mut buf).is_ok() {
        if s.write_all(&[0u8; ANSWER]).is_err() {
            break;
        }
    }
}

fn ping(c: &mut (impl Read + Write)) {
    c.write_all(&[1u8; REQUEST]).unwrap();
    let mut buf = [0u8; ANSWER];
    c.read_exact(&mut buf).unwrap();
}

#[test]
#[ignore]
fn where_a_read_by_key_spends_its_time() {
    let n = rows();
    let dir = std::env::temp_dir().join(format!("nucleus-phases-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let kv: Arc<dyn Kv> = Arc::new(Database::open(&dir, Options::default()).unwrap());
    let mut s = Session::new(kv);
    s.execute("CREATE TABLE b (id INT PRIMARY KEY, g INT, v BIGINT, s TEXT)")
        .unwrap();
    s.execute(&format!(
        "INSERT INTO b SELECT i, i % 1000, i::bigint * 7919 % 1000003, 'row ' || i \
         FROM generate_series(1, {n}) i"
    ))
    .unwrap();
    s.execute("ANALYZE b").unwrap();
    let texts: Vec<String> = (0..READS)
        .map(|i| format!("SELECT s FROM b WHERE id = {}", key(i, n)))
        .collect();
    // Warm: every page the reads touch in the cache.
    for t in &texts {
        s.execute(t).unwrap();
    }

    // The whole statement in the process, its text new each time (as
    // the suite's: a literal key).
    let literal = per_call(|i| {
        let out = s.execute(&texts[i]).unwrap();
        assert!(matches!(&out[0], Output::Rows { rows, .. } if rows.len() == 1));
    });
    // Parsing alone.
    let parse = per_call(|i| {
        nucleus_sql::parse(&texts[i]).unwrap();
    });
    // Parsed once, the key a parameter: binding, planning, running.
    let stmt = nucleus_sql::parse("SELECT s FROM b WHERE id = $1")
        .unwrap()
        .remove(0);
    let param = per_call(|i| {
        s.run(&stmt, &[Value::Int(key(i, n) as i64)]).unwrap();
    });
    // The same text each time: the session's parse cache's case.
    let same = per_call(|_| {
        s.execute(&texts[0]).unwrap();
    });
    // Binding and planning without running: EXPLAIN of the statement.
    let explain_stmts: Vec<_> = texts
        .iter()
        .map(|t| {
            nucleus_sql::parse(&format!("EXPLAIN {t}"))
                .unwrap()
                .remove(0)
        })
        .collect();
    let plan = per_call(|i| {
        s.run(&explain_stmts[i], &[]).unwrap();
    });
    // A statement that reads no table: the session's own cost.
    let one = nucleus_sql::parse("SELECT 1").unwrap().remove(0);
    let select1 = per_call(|_| {
        s.run(&one, &[]).unwrap();
    });
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);

    let tcp = round_trips_tcp();
    let unix = round_trips_unix();

    report("a read by key, literal, in the process", literal);
    report("parsing it", parse);
    report("a read by key, a parameter, in the process", param);
    report("a read by key, the same text again, in the process", same);
    report("EXPLAIN of it (binding and planning), in the process", plan);
    report("SELECT 1, in the process", select1);
    report("a round trip, loopback TCP", tcp);
    report("a round trip, Unix socket", unix);
}
