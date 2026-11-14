//! Branches measured (PLAN.md §M14.13 B5): real files, release build.
//!
//!     cargo run --release -p nucleus-kv --example branches [<dir>]
//!
//! For databases of growing size: the time and the space a branch
//! takes when made, when dropped untouched, as of a past commit (a pass
//! over its entries) and reset; then, on the largest, a branch changed
//! by more and more rows — the space its changes take and the time its
//! drop takes.  Rows of a 16-byte key and a 100-byte value, loaded in
//! 10,000-row commits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nucleus_kv::{Database, Kv, Options, TxnOptions, transact};

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

/// The bytes of the data file's pages in use (all branches').
fn used(db: &Database) -> i64 {
    let (pages, free) = db.pages();
    ((pages - free) * nucleus_storage::PAGE_SIZE as u64) as i64
}

fn mib(b: i64) -> String {
    format!("{:.1} MiB", b as f64 / (1 << 20) as f64)
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

fn load(db: &dyn Kv, from: u64, to: u64, value: &[u8]) {
    let mut i = from;
    while i < to {
        let end = (i + 10_000).min(to);
        transact(db, TxnOptions::default(), |txn| {
            for k in i..end {
                txn.put(&key(k), value)?;
            }
            Ok(())
        })
        .unwrap();
        i = end;
    }
}

/// Rewrites `n` rows spread evenly over the first `of` keys (`of == n`:
/// the first `n`), in 10,000-row commits.
fn change(db: &dyn Kv, n: u64, of: u64, value: &[u8]) {
    let step = (of / n).max(1);
    let mut done = 0;
    while done < n {
        let end = (done + 10_000).min(n);
        transact(db, TxnOptions::default(), |txn| {
            for j in done..end {
                txn.put(&key(j * step), value)?;
            }
            Ok(())
        })
        .unwrap();
        done = end;
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("nucleus-branches-{}", std::process::id()))
        });
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(
        &dir,
        Options {
            cache_bytes: 256 << 20,
            history_time: Duration::from_secs(3600),
            ..Options::default()
        },
    )
    .unwrap();
    let value = vec![b'v'; 100];
    let newer = vec![b'w'; 100];

    println!("rows        in use      create   +space     drop   as-of(10k later)  reset");
    let mut rows = 0;
    for n in [100_000u64, 1_000_000, 4_000_000] {
        load(&db, rows, n, &value);
        rows = n;
        // A past snapshot: 10,000 rows' changes ago.
        let at = db.begin(TxnOptions::default()).unwrap().read_ts();
        let then = transact(&db, TxnOptions::default(), |txn| txn.get(&key(0))).unwrap();
        change(&db, 10_000, n, &newer);
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        let before = used(&db);

        let t = Instant::now();
        let b = db.create_branch("b").unwrap();
        let create = t.elapsed();
        db.checkpoint().unwrap();
        let space = used(&db) - before;
        drop(b);
        let t = Instant::now();
        db.drop_branch("b").unwrap();
        let dropped = t.elapsed();

        let t = Instant::now();
        let past = db.create_branch_at("past", at).unwrap();
        let as_of = t.elapsed();
        assert_eq!(
            transact(&*past, TxnOptions::default(), |txn| txn.get(&key(0))).unwrap(),
            then
        );
        drop(past);
        let t = Instant::now();
        let past = db.reset_branch("past").unwrap();
        let reset = t.elapsed();
        drop(past);
        db.drop_branch("past").unwrap();
        db.checkpoint().unwrap();

        println!(
            "{n:>9}  {:>10}  {:>8}  {:>8}  {:>8}  {:>12}  {:>9}",
            mib(before),
            ms(create),
            mib(space),
            ms(dropped),
            ms(as_of),
            ms(reset)
        );
    }

    // The branch's changes: their space, its drop's time and the space
    // the drop gives back — rows next to each other, then spread over
    // the whole table (a leaf each, every internal node above copied).
    println!("\nrows changed on the branch        +space     drop    freed");
    for (k, spread) in [
        (1_000u64, false),
        (100_000, false),
        (1_000, true),
        (10_000, true),
        (100_000, true),
        (1_000_000, true),
    ] {
        let b: Arc<dyn Kv> = db.create_branch("b").unwrap();
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        let before = used(&db);
        change(&*b, k, if spread { rows } else { k }, &newer);
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        let after = used(&db);
        drop(b);
        let t = Instant::now();
        db.drop_branch("b").unwrap();
        let dropped = t.elapsed();
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        println!(
            "{k:>9} {:>10}  {:>14}  {:>8}  {:>8}",
            if spread { "spread" } else { "together" },
            mib(after - before),
            ms(dropped),
            mib(after - used(&db))
        );
    }
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
