//! The online type change's measurement (PLAN.md §M14.18 O5;
//! docs/benchmarks.md "Changing a column's type"), on real files — not run
//! by default:
//!
//! ```sh
//! RETYPE_DIR=/abs/retype-bench RETYPE_ROWS=1000000 cargo test --release \
//!     -p nucleus-sql --test retype_bench -- --ignored --nocapture
//! ```
//!
//! (An absolute `RETYPE_DIR`: a relative one is the test's, under
//! `crates/sql/`.)  Without `RETYPE_DIR` (the CI's run of ignored tests)
//! it runs as a test: 20 000 rows in a temporary directory, removed
//! after.
//!
//! A table of `RETYPE_ROWS` rows (`id` the key, `g`, `v` indexed, `s`
//! text); `v` changed from `integer` to `bigint` and back, `RUNS` times
//! each way: in one transaction and `CONCURRENTLY`, on an idle table and
//! while another session updates single rows by key as fast as it can —
//! its latencies with no change running, and during each form.  Each
//! figure is the median of the runs, with the fastest and the slowest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nucleus_kv::{Database, Kv, Options};
use nucleus_sql::{Session, TxnStatus};

const RUNS: usize = 5;
/// Tries of a form that meets the writer's conflicts, at most.
const TRIES: usize = 3;

/// The median, the fastest and the slowest.
fn spread(mut xs: Vec<f64>) -> String {
    xs.sort_by(f64::total_cmp);
    format!(
        "{:.3} (from {:.3} to {:.3})",
        xs[xs.len() / 2],
        xs[0],
        xs[xs.len() - 1]
    )
}

fn quantile(sorted: &[Duration], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

/// A writer updating single rows by key, retried on conflicts, until
/// `stop`: its latencies (each statement's, retries included) and how
/// many conflicts it met.
fn writer(kv: Arc<dyn Kv>, rows: u64, stop: Arc<AtomicBool>) -> (Vec<Duration>, u64) {
    let mut s = Session::new(kv);
    let mut lat = Vec::new();
    let mut conflicts = 0;
    let mut i = 0u64;
    while !stop.load(Ordering::SeqCst) {
        i += 1;
        let k = (i * 7919) % rows + 1;
        let sql = format!("UPDATE t SET g = g + 1, v = {} WHERE id = {k}", i % 1000);
        let at = Instant::now();
        loop {
            match s.execute(&sql) {
                Ok(_) => break,
                Err(e) => {
                    if s.status() != TxnStatus::Idle {
                        let _ = s.execute("ROLLBACK");
                    }
                    assert_eq!(e.code, "40001", "{sql}: {e:?}");
                    conflicts += 1;
                }
            }
        }
        lat.push(at.elapsed());
    }
    lat.sort();
    (lat, conflicts)
}

#[test]
#[ignore]
fn type_change() {
    let given = std::env::var("RETYPE_DIR").ok();
    let rows: u64 = std::env::var("RETYPE_ROWS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(20_000);
    let dir = match &given {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::temp_dir().join(format!("nucleus-retype-{}", std::process::id())),
    };
    let dir = dir.as_path();
    assert!(
        !dir.exists(),
        "{} exists: name a new directory",
        dir.display()
    );
    let kv: Arc<dyn Kv> = Arc::new(Database::open(dir, Options::default()).unwrap());
    let mut s = Session::new(kv.clone());
    s.execute(&format!(
        "CREATE TABLE t (id INT PRIMARY KEY, g INT, v INT, s TEXT); \
         CREATE INDEX t_v ON t (v); \
         INSERT INTO t SELECT i, i % 11, i * 7 % 100000, 'value ' || i \
         FROM generate_series(1, {rows}) i"
    ))
    .unwrap();
    println!("{rows} rows, an index on the column");
    let types = ["BIGINT", "INT"];
    let mut ty = 0;
    let mut next = || {
        ty = 1 - ty;
        types[1 - ty]
    };

    // Idle: each form's time.
    for form in ["", " CONCURRENTLY"] {
        let mut secs = Vec::new();
        for _ in 0..RUNS {
            let at = Instant::now();
            s.execute(&format!(
                "ALTER TABLE t ALTER COLUMN v TYPE {}{form}",
                next()
            ))
            .unwrap();
            secs.push(at.elapsed().as_secs_f64());
        }
        println!("idle, ALTER COLUMN … TYPE{form}: {} s", spread(secs));
    }

    // A writer alone: its latencies with no change running.
    let run_writer = |kv: &Arc<dyn Kv>| {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let kv = Arc::clone(kv);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || writer(kv, rows, stop))
        };
        (stop, handle)
    };
    let (stop, handle) = run_writer(&kv);
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::SeqCst);
    let (lat, _) = handle.join().unwrap();
    println!(
        "a writer alone: {:.0} updates/s, p50 {:.3} ms, p99 {:.3} ms",
        lat.len() as f64 / 2.0,
        quantile(&lat, 0.5),
        quantile(&lat, 0.99)
    );

    // Under writes: each form's time, whether it finishes, and the
    // writer's latencies meanwhile.
    for form in ["", " CONCURRENTLY"] {
        let (mut secs, mut p50, mut p99, mut rate) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut failed = 0;
        for _ in 0..RUNS {
            let (stop, handle) = run_writer(&kv);
            std::thread::sleep(Duration::from_millis(200));
            let at = Instant::now();
            let to = next();
            // The transactional form conflicts with the writer: tried
            // again, up to a limit.
            let mut done = false;
            for _ in 0..TRIES {
                match s.execute(&format!("ALTER TABLE t ALTER COLUMN v TYPE {to}{form}")) {
                    Ok(_) => {
                        done = true;
                        break;
                    }
                    Err(e) if e.code == "40001" => {}
                    Err(e) => panic!("{e:?}"),
                }
            }
            let took = at.elapsed();
            stop.store(true, Ordering::SeqCst);
            let (lat, _) = handle.join().unwrap();
            secs.push(took.as_secs_f64());
            p50.push(quantile(&lat, 0.5));
            p99.push(quantile(&lat, 0.99));
            rate.push(lat.len() as f64 / took.as_secs_f64());
            if !done {
                failed += 1;
                // The type the next run expects, with no one writing.
                s.execute(&format!("ALTER TABLE t ALTER COLUMN v TYPE {to}"))
                    .unwrap();
            }
        }
        println!(
            "under writes, ALTER COLUMN … TYPE{form}: {} s, {failed} of {RUNS} not done in {TRIES} tries; \
             the writer: {} updates/s, p50 {} ms, p99 {} ms",
            spread(secs),
            spread(rate),
            spread(p50),
            spread(p99)
        );
    }

    let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
    drop(txn);
    drop(s);
    drop(kv);
    if given.is_none() {
        std::fs::remove_dir_all(dir).unwrap();
    }
}
