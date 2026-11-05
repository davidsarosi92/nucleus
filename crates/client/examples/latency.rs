//! What a statement costs end to end, one at a time on one connection
//! (PLAN.md §M14.20 W8): `cargo run --release -p nucleus-client --example
//! latency -- PORT [N] [ONLY]` against a running server (`--no-auth`).
//! Each kind of statement (or those whose name holds ONLY) runs N times
//! (default 20,000); printed are the median and the 99th percentile in µs
//! and the statements a second.

use std::time::Instant;

use nucleus_client::{Client, Config, Format, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).expect("a port");
    let n: usize = args.next().and_then(|p| p.parse().ok()).unwrap_or(20_000);
    let only = args.next().unwrap_or_default();
    let mut c = Client::connect(Config {
        port,
        password: None,
        ..Config::default()
    })
    .expect("connects");
    let run = |c: &mut Client, sql: &str, params: &[Value]| {
        for r in c.query(sql, params, Format::Typed).expect("runs") {
            r.expect("succeeds");
        }
    };
    run(&mut c, "DROP TABLE IF EXISTS lat_kv", &[]);
    run(
        &mut c,
        "CREATE TABLE lat_kv (k BIGINT PRIMARY KEY, v TEXT, n INT)",
        &[],
    );
    run(
        &mut c,
        "INSERT INTO lat_kv SELECT g, 'value ' || g, g % 100 FROM generate_series(1, 100000) g",
        &[],
    );
    let mut next = 1_000_000i64;
    type Case<'a> = (&'a str, Box<dyn FnMut(usize) -> (String, Vec<Value>) + 'a>);
    let cases: Vec<Case> = vec![
        ("SELECT 1", Box::new(|_| ("SELECT 1".into(), vec![]))),
        (
            "point read, literal",
            Box::new(|i| {
                (
                    format!("SELECT v, n FROM lat_kv WHERE k = {}", i % 100_000 + 1),
                    vec![],
                )
            }),
        ),
        (
            "point read, parameter",
            Box::new(|i| {
                (
                    "SELECT v, n FROM lat_kv WHERE k = $1".into(),
                    vec![Value::Int((i % 100_000 + 1) as i64)],
                )
            }),
        ),
        (
            "insert, parameters",
            Box::new(move |_| {
                next += 1;
                (
                    "INSERT INTO lat_kv VALUES ($1, $2, $3)".into(),
                    vec![Value::Int(next), Value::Text("new".into()), Value::Int(7)],
                )
            }),
        ),
        (
            "update by key, parameters",
            Box::new(|i| {
                (
                    "UPDATE lat_kv SET n = n + 1 WHERE k = $1".into(),
                    vec![Value::Int((i % 100_000 + 1) as i64)],
                )
            }),
        ),
    ];
    for (name, mut make) in cases {
        if !name.contains(&only) {
            continue;
        }
        for i in 0..n / 10 {
            let (sql, p) = make(i);
            run(&mut c, &sql, &p);
        }
        let mut times = Vec::with_capacity(n);
        let all = Instant::now();
        for i in 0..n {
            let (sql, p) = make(i);
            let t = Instant::now();
            run(&mut c, &sql, &p);
            times.push(t.elapsed().as_nanos() as u64);
        }
        let total = all.elapsed().as_secs_f64();
        times.sort_unstable();
        println!(
            "{name:28} median {:7.1} µs  p99 {:7.1} µs  {:8.0} /s",
            times[n / 2] as f64 / 1e3,
            times[n * 99 / 100] as f64 / 1e3,
            n as f64 / total
        );
    }
    run(&mut c, "DROP TABLE lat_kv", &[]);
}
