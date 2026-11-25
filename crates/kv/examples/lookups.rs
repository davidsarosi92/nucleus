//! Reads by key on a cached tree, on one thread and on several sharing a
//! transaction's reads (a parallel lookup join's workers): the cost of
//! one, and how it scales (PLAN.md, the perf phase's point reads).
//!
//!     cargo run --release -p nucleus-kv --example lookups [<rows>] [<reads a thread>] [<threads,…>]
//!
//! `<rows>` keys (default 2,000,000) of a table's shape — a 4-byte table
//! prefix and an 8-byte big-endian id — with 40-byte values, loaded and
//! read once whole (every page cached); then, for 1, 2, 4, 8 and 10
//! threads, each thread reads `<reads a thread>` keys (default 1,000,000)
//! at random.  Printed: the nanoseconds a read on a thread, the reads a
//! second in all, and the speed-up over one thread; `@metric` lines too,
//! for `bench/method.py run`.

use std::time::Instant;

use nucleus_kv::{Database, Kv, Options, TxnOptions};

fn key(i: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&7u32.to_be_bytes());
    k[4..].copy_from_slice(&i.to_be_bytes());
    k
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rows: u64 = args
        .next()
        .map_or(2_000_000, |s| s.parse().expect("<rows>"));
    let reads: u64 = args
        .next()
        .map_or(1_000_000, |s| s.parse().expect("<reads a thread>"));
    let counts: Vec<u64> = args.next().map_or(vec![1, 2, 4, 8, 10], |s| {
        s.split(',')
            .map(|n| n.parse().expect("<threads,…>"))
            .collect()
    });
    let dir = std::env::temp_dir().join(format!("nucleus-lookups-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(
        &dir,
        Options {
            cache_bytes: 1 << 30,
            ..Options::default()
        },
    )
    .unwrap();
    let value = [b'v'; 40];
    let t = Instant::now();
    let batch = 100_000;
    for from in (0..rows).step_by(batch as usize) {
        let mut txn = db.begin(TxnOptions::default()).unwrap();
        for i in from..(from + batch).min(rows) {
            txn.put(&key(i), &value).unwrap();
        }
        txn.commit().unwrap();
    }
    println!("loaded {rows} rows in {:.2} s", t.elapsed().as_secs_f64());

    let txn = db.begin(TxnOptions::read_only()).unwrap();
    let all = txn
        .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap()
        .count() as u64;
    assert_eq!(all, rows);
    let shared = txn.shared().expect("a transaction's reads shared");
    // Warm: every leaf on the path of a read.
    for i in 0..rows.min(200_000) {
        assert!(shared.get(&key(i * 7919 % rows)).unwrap().is_some());
    }
    let mut one = 0.0;
    for &threads in &counts {
        let t = Instant::now();
        std::thread::scope(|s| {
            for th in 0..threads {
                s.spawn(move || {
                    // xorshift, a stream a thread.
                    let mut x =
                        0x9E37_79B9_7F4A_7C15u64 ^ (th + 1).wrapping_mul(0xD1B5_4A32_D192_ED03);
                    for _ in 0..reads {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let v = shared.get(&key(x % rows)).unwrap();
                        assert!(v.is_some());
                    }
                });
            }
        });
        let s = t.elapsed().as_secs_f64();
        let rate = (threads * reads) as f64 / s;
        if one == 0.0 {
            one = rate * counts[0] as f64 / threads as f64;
        }
        let ns = s * 1e9 / reads as f64;
        println!(
            "{threads:>2} threads: {ns:>7.0} ns a read on a thread, {rate:>11.0} reads/s, {:.2}× one thread",
            rate / one
        );
        metric(&format!("lookup ns, {threads} threads"), ns, "ns");
    }
    drop(txn);
    db.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A number on stdout as `bench/method.py run` gathers them (PLAN.md
/// §M14.31).
fn metric(name: &str, value: f64, unit: &str) {
    println!(
        "@metric {{\"name\": {name:?}, \"values\": [{value}], \"unit\": {unit:?}, \
         \"system\": \"nucleus\", \"phase\": \"warm\"}}"
    );
}
