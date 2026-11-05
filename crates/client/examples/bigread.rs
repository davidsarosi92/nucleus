//! Reading a large result over one connection (PLAN.md §M14.20 W8):
//! `cargo run --release -p nucleus-client --example bigread -- PORT [ROWS]`
//! against a running server (`--no-auth`).  A table of ROWS rows (default
//! 1,000,000) of a bigint, a text, a numeric and a timestamp is read
//! whole, as rows and as columnar batches, five times each; printed is
//! the median in ms.

use std::time::Instant;

use nucleus_client::{Client, Config, Format, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).expect("a port");
    let n: u64 = args
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1_000_000);
    let mut c = Client::connect(Config {
        port,
        password: None,
        ..Config::default()
    })
    .expect("connects");
    c.execute("DROP TABLE IF EXISTS big_read", &[])
        .expect("drops");
    c.execute(
        "CREATE TABLE big_read (k BIGINT PRIMARY KEY, v TEXT, n NUMERIC(12, 2), t TIMESTAMP)",
        &[],
    )
    .expect("creates");
    c.execute(
        "INSERT INTO big_read SELECT g, 'value ' || g, g / 7.0, TIMESTAMP '2024-01-01' + g * INTERVAL '1 second' FROM generate_series(1, $1) g",
        &[Value::Int(n as i64)],
    )
    .expect("fills");
    let median = |mut t: Vec<f64>| {
        t.sort_by(f64::total_cmp);
        t[t.len() / 2]
    };
    let sql = "SELECT * FROM big_read";
    let mut rows = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let r = c.query(sql, &[], Format::Typed).expect("reads");
        assert_eq!(r[0].as_ref().expect("ok").rows.len() as u64, n);
        rows.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut batches = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut b = c.batches(sql, &[], 0).expect("reads");
        let mut got = 0u64;
        for batch in &mut b {
            got += batch.expect("a batch").rows() as u64;
        }
        assert_eq!(got, n);
        batches.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{n} rows: as rows {:.0} ms, as columnar batches {:.0} ms",
        median(rows),
        median(batches)
    );
    c.execute("DROP TABLE big_read", &[]).expect("drops");
}
