//! What a prepared statement costs end to end through the PostgreSQL
//! front end, one at a time on one connection (PLAN.md §M14.20 W8):
//! `cargo run --release -p nucleus-pg-compat --example pg_latency -- PORT [N]`
//! against `nucleus server DIR --no-auth --pg-compat 127.0.0.1:PORT`.
//! Each runs N times (default 20,000); printed are the median and the
//! 99th percentile in µs and the statements a second.

use std::time::Instant;

use postgres::{Client, NoTls};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).expect("a port");
    let n: usize = args.next().and_then(|p| p.parse().ok()).unwrap_or(20_000);
    let mut c = Client::connect(
        &format!("host=127.0.0.1 port={port} user=nucleus dbname=nucleus"),
        NoTls,
    )
    .expect("connects");
    c.batch_execute(
        "DROP TABLE IF EXISTS lat_pg; \
         CREATE TABLE lat_pg (k BIGINT PRIMARY KEY, v TEXT, n INT); \
         INSERT INTO lat_pg SELECT g, 'value ' || g, g % 100 FROM generate_series(1, 100000) g",
    )
    .expect("loads");
    let read = c
        .prepare("SELECT v, n FROM lat_pg WHERE k = $1")
        .expect("prepares");
    let update = c
        .prepare("UPDATE lat_pg SET n = n + 1 WHERE k = $1")
        .expect("prepares");
    let mut cases: Vec<(&str, Box<dyn FnMut(&mut Client, i64)>)> = vec![
        (
            "prepared point read",
            Box::new(|c, k| {
                c.query_one(&read, &[&k]).expect("reads");
            }),
        ),
        (
            "prepared update by key",
            Box::new(|c, k| {
                c.execute(&update, &[&k]).expect("updates");
            }),
        ),
    ];
    for (name, run) in cases.iter_mut() {
        let mut times = Vec::with_capacity(n);
        let start = Instant::now();
        for i in 0..n {
            let t = Instant::now();
            run(&mut c, (i % 100_000 + 1) as i64);
            times.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let total = start.elapsed().as_secs_f64();
        times.sort_by(f64::total_cmp);
        println!(
            "{name:<28} median {:>7.1} µs  p99 {:>7.1} µs  {:>8.0} /s",
            times[n / 2],
            times[n * 99 / 100],
            n as f64 / total
        );
    }
}
