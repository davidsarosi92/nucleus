//! YCSB's core workloads on either engine (PLAN.md §M14.28 L5): a table
//! of `R` rows — a `bigint` key and 100 bytes of text, as
//! `bench/wamp/wamp.py`'s — loaded and checkpointed, then workloads A
//! (50 % reads, 50 % updates), B (95 % reads, 5 % updates) and E (95 %
//! short scans of 1–100 rows, 5 % inserts), keys chosen by YCSB's
//! scrambled Zipfian (θ = 0.99), each operation a statement in a
//! transaction of its own through a prepared statement; the background
//! checkpoints and compactions running as they do.  Throughput, and each
//! kind of operation's p50 and p99.  Real files; one client.
//!
//! `cargo test --release -p nucleus-sql --test ycsb -- --ignored --nocapture`
//!
//! `YCSB_ROWS` (default 1,000,000) and `YCSB_OPS` (default 200,000)
//! scale it; `YCSB_ENGINES` (default `btree,lsm`) chooses the engines.  Each figure is printed as an `@metric` line too, for
//! `bench/method.py run`.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session, Value};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// YCSB's Zipfian over `0..n` (Gray et al., "Quickly generating
/// billion-record synthetic databases", SIGMOD 1994), scrambled by
/// FNV-1a as YCSB's `ScrambledZipfianGenerator` is, so the hot keys are
/// spread over the key space.
struct Zipf {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
}

impl Zipf {
    fn new(n: u64, theta: f64) -> Self {
        let zeta = |n: u64| (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum::<f64>();
        let (zeta2, zetan) = (zeta(2), zeta(n));
        Self {
            n,
            theta,
            alpha: 1.0 / (1.0 - theta),
            zetan,
            eta: (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan),
        }
    }

    fn rank(&self, rng: &SplitMix64) -> u64 {
        let u = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(self.theta) {
            return 1;
        }
        ((self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64).min(self.n - 1)
    }

    /// A key of `0..n`, the ranks scrambled.
    fn key(&self, rng: &SplitMix64) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.rank(rng).to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h % self.n
    }
}

fn value(rng: &SplitMix64) -> Value {
    let s: String = (0..100)
        .map(|_| char::from(b'a' + rng.below(26) as u8))
        .collect();
    Value::Text(s)
}

/// Latencies in µs: p50, p99.
fn quantiles(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[v.len() * 99 / 100])
}

fn metric(name: &str, value: f64, unit: &str) {
    println!(
        "@metric {{\"name\": \"{name}\", \"values\": [{value}], \"unit\": \"{unit}\", \"system\": \"nucleus\", \"phase\": \"ycsb\"}}"
    );
}

fn run(engine: &str) {
    let rows = env("YCSB_ROWS", 1_000_000);
    let ops = env("YCSB_OPS", 200_000);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ycsb-{engine}"));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(&dir, Options::default()).unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(&format!(
        "CREATE TABLE usertable (k BIGINT PRIMARY KEY, v TEXT) WITH (engine = {engine})"
    ))
    .unwrap();
    let t = Instant::now();
    let chunk = 100_000;
    for from in (0..rows).step_by(chunk as usize) {
        s.execute(&format!(
            "INSERT INTO usertable SELECT g, repeat('x', 100) \
             FROM generate_series({from}, {}) g",
            (from + chunk).min(rows) - 1
        ))
        .unwrap();
    }
    db.checkpoint().unwrap();
    let load = t.elapsed().as_secs_f64();
    println!("{engine}: {rows} rows loaded in {load:.1} s");
    metric(&format!("{engine}: load {rows} rows"), load, "s");

    let stmt = |sql: &str| nucleus_sql::parser::parse(sql).unwrap().remove(0);
    let read = stmt("SELECT v FROM usertable WHERE k = $1");
    let update = stmt("UPDATE usertable SET v = $2 WHERE k = $1");
    let insert = stmt("INSERT INTO usertable VALUES ($1, $2)");
    let scan = stmt("SELECT k, v FROM usertable WHERE k >= $1 ORDER BY k LIMIT $2");
    let zipf = Zipf::new(rows, 0.99);
    let rng = SplitMix64::new(42);
    let mut next_insert = rows;
    for (name, reads, other) in [
        ("A", 50, "update"),
        ("B", 95, "update"),
        ("E", 95, "insert"),
    ] {
        let mut lat: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        let t = Instant::now();
        for _ in 0..ops {
            let is_read = rng.below(100) < reads;
            let o = Instant::now();
            if is_read && name == "E" {
                let len = 1 + rng.below(100) as i64;
                let k = zipf.key(&rng) as i64;
                match s.run(&scan, &[Value::Int(k), Value::Int(len)]).unwrap() {
                    Output::Rows { rows, .. } => assert!(!rows.is_empty(), "key {k}"),
                    other => panic!("{other:?}"),
                }
            } else if is_read {
                let k = zipf.key(&rng) as i64;
                match s.run(&read, &[Value::Int(k)]).unwrap() {
                    Output::Rows { rows, .. } => assert_eq!(rows.len(), 1, "key {k}"),
                    other => panic!("{other:?}"),
                }
            } else if other == "update" {
                let k = zipf.key(&rng) as i64;
                s.run(&update, &[Value::Int(k), value(&rng)]).unwrap();
            } else {
                s.run(&insert, &[Value::Int(next_insert as i64), value(&rng)])
                    .unwrap();
                next_insert += 1;
            }
            lat[usize::from(!is_read)].push(o.elapsed().as_secs_f64() * 1e6);
        }
        let secs = t.elapsed().as_secs_f64();
        let first = if name == "E" { "scan" } else { "read" };
        let (r50, r99) = quantiles(std::mem::take(&mut lat[0]));
        let (w50, w99) = quantiles(std::mem::take(&mut lat[1]));
        println!(
            "{engine}: workload {name}: {:.0} ops/s; {first} p50 {r50:.1} µs, p99 {r99:.1} µs; \
             {other} p50 {w50:.1} µs, p99 {w99:.1} µs",
            ops as f64 / secs
        );
        metric(
            &format!("{engine}: workload {name}"),
            ops as f64 / secs,
            "ops/s",
        );
        metric(
            &format!("{engine}: workload {name}, {first} p50"),
            r50,
            "µs",
        );
        metric(
            &format!("{engine}: workload {name}, {first} p99"),
            r99,
            "µs",
        );
        metric(
            &format!("{engine}: workload {name}, {other} p50"),
            w50,
            "µs",
        );
        metric(
            &format!("{engine}: workload {name}, {other} p99"),
            w99,
            "µs",
        );
    }
    drop(s);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "a measurement: run it in release"]
fn ycsb_on_both_engines() {
    let engines = std::env::var("YCSB_ENGINES").unwrap_or_else(|_| "btree,lsm".into());
    for e in engines.split(',') {
        run(e);
    }
}
