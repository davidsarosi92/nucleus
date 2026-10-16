//! A first benchmark baseline (PLAN.md §M6): real files, release build.
//!
//!     cargo run --release -p nucleus-kv --example bench [<dir>]
//!
//! Single-row commits (each waits for an fsync), batched commits, group
//! commit from several threads, point reads, and a full scan.

use std::ops::Bound;
use std::time::Instant;

use nucleus_kv::{Database, Kv, Options, TxnOptions, transact};

fn rate(n: u64, start: Instant) -> String {
    let s = start.elapsed().as_secs_f64();
    format!("{n:>8} in {s:>7.3} s = {:>10.0} /s", n as f64 / s)
}

fn key(i: u64) -> Vec<u8> {
    format!("user{i:010}").into_bytes()
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("nucleus-bench-{}", std::process::id()))
        });
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(
        &dir,
        Options {
            cache_bytes: 64 << 20,
            ..Options::default()
        },
    )
    .unwrap();
    let value = vec![b'v'; 100];

    let n = 1_000;
    let t = Instant::now();
    for i in 0..n {
        transact(&db, TxnOptions::default(), |txn| txn.put(&key(i), &value)).unwrap();
    }
    println!("single-row commits      {}", rate(n, t));

    let (batches, per) = (200, 500);
    let t = Instant::now();
    for b in 0..batches {
        transact(&db, TxnOptions::default(), |txn| {
            for i in 0..per {
                txn.put(&key(n + b * per + i), &value)?;
            }
            Ok(())
        })
        .unwrap();
    }
    let total = n + batches * per;
    println!("rows in 500-row commits {}", rate(batches * per, t));

    let threads = 8;
    let t = Instant::now();
    std::thread::scope(|s| {
        for th in 0..threads {
            let db = db.clone();
            let value = &value;
            s.spawn(move || {
                for i in 0..250u64 {
                    let k = key(total + th * 1000 + i);
                    transact(&db, TxnOptions::default(), |txn| txn.put(&k, value)).unwrap();
                }
            });
        }
    });
    println!("group commit, 8 threads {}", rate(threads * 250, t));

    let reads = 200_000;
    let txn = db.begin(TxnOptions::read_only()).unwrap();
    let t = Instant::now();
    for i in 0..reads {
        assert!(txn.get(&key(i * 7919 % total)).unwrap().is_some());
    }
    println!("point reads             {}", rate(reads, t));

    let t = Instant::now();
    let rows = txn
        .scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .count() as u64;
    println!("full scan               {}", rate(rows, t));
    drop(txn);
    db.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
