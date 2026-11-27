//! Changefeeds (PLAN.md §M16.5 S3) beyond `slt/98_changefeeds.slt`: a
//! feed made while writers run, with a copy of its table in the same
//! transaction, replayed onto the copy in `seq` order, gives the table —
//! every committed change in it once, in commit order, none rolled back,
//! none missed around the feed's making — on every storage.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session};

/// The storages a table's rows may have.
const STORAGES: &[&str] = &["", "WITH (storage = column)", "WITH (engine = lsm)"];

fn open(seed: u64) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

/// `sql`'s rows, each column an integer or `NULL` (`None`).
fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<i64>>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Int(i) => Some(*i),
                        Value::Null => None,
                        v => panic!("{sql}: {v:?}"),
                    })
                    .collect()
            })
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

/// Runs `sql` until no conflict refuses it.
fn retried(s: &mut Session, sql: &str) {
    loop {
        match s.execute(sql) {
            Ok(_) => return,
            Err(e) if e.code == "40001" => {
                let _ = s.execute("ROLLBACK");
            }
            Err(e) => panic!("{sql}: {} {}", e.code, e.message),
        }
    }
}

fn replayed(storage: &str, seed: u64) {
    const WRITERS: u64 = 4;
    const EACH: u64 = 120;
    const KEYS: u64 = 16;
    let kv = open(seed);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(&format!(
        "CREATE TABLE t (k INT PRIMARY KEY, v INT, note TEXT) {storage}; \
         CREATE TABLE copy (k INT PRIMARY KEY, v INT, note TEXT)"
    ))
    .unwrap();
    let made = AtomicBool::new(false);
    let done = AtomicU64::new(0);
    std::thread::scope(|sc| {
        for w in 0..WRITERS {
            let kv = Arc::clone(&kv);
            let (done, made) = (&done, &made);
            sc.spawn(move || {
                let mut s = Session::new(kv);
                let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(w + 1);
                for i in 0..EACH {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let k = (x >> 33) % KEYS;
                    let sql = match (x >> 20) % 6 {
                        0 | 1 => format!(
                            "INSERT INTO t VALUES ({k}, {i}, 'w{w}') \
                             ON CONFLICT (k) DO UPDATE SET v = t.v + 1, note = excluded.note"
                        ),
                        2 => format!("UPDATE t SET v = v * 2 WHERE k = {k}"),
                        3 => format!("DELETE FROM t WHERE k = {k}"),
                        // Two rows in one transaction.
                        4 => format!(
                            "BEGIN; INSERT INTO t VALUES ({k}, 0, NULL) ON CONFLICT DO NOTHING; \
                             UPDATE t SET v = v + 100 WHERE k = {}; COMMIT",
                            (k + 1) % KEYS
                        ),
                        // Rolled back: in no feed.
                        _ => format!("BEGIN; DELETE FROM t WHERE k < {k}; ROLLBACK"),
                    };
                    retried(&mut s, &sql);
                    done.fetch_add(1, Ordering::SeqCst);
                    // The last third waits for the feed: written after it.
                    while i == EACH * 2 / 3 && !made.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
            });
        }
        // The feed made while they write, its table's rows copied in the
        // same transaction.
        let kv = Arc::clone(&kv);
        let (done, made) = (&done, &made);
        sc.spawn(move || {
            let mut s = Session::new(kv);
            while done.load(Ordering::SeqCst) < WRITERS * EACH / 3 {
                std::thread::yield_now();
            }
            retried(
                &mut s,
                "BEGIN ISOLATION LEVEL REPEATABLE READ; \
                 CREATE CHANGEFEED f ON t WITH (before); \
                 INSERT INTO copy SELECT * FROM t; COMMIT",
            );
            made.store(true, Ordering::SeqCst);
        });
    });
    assert!(made.load(Ordering::SeqCst));
    // Replayed in seq order: a delete under its key, then the row after.
    let changes = rows(
        &mut s,
        "SELECT seq, (key ->> 'k')::int, (after ->> 'k')::int, (after ->> 'v')::int, \
                (before ->> 'k')::int, CASE op WHEN 'insert' THEN 1 WHEN 'update' THEN 2 ELSE 3 END \
         FROM f ORDER BY seq",
    );
    for (i, c) in changes.iter().enumerate() {
        assert_eq!(c[0], Some(i as i64 + 1), "dense: {c:?}");
        let key = c[1].unwrap();
        match c[5] {
            Some(1) => assert!(c[2] == Some(key) && c[4].is_none(), "{c:?}"),
            Some(2) => assert!(c[2] == Some(key) && c[4] == Some(key), "{c:?}"),
            _ => assert!(c[2].is_none() && c[4] == Some(key), "{c:?}"),
        }
        s.execute(&format!("DELETE FROM copy WHERE k = {key}"))
            .unwrap();
        if c[2].is_some() {
            s.execute(&format!(
                "INSERT INTO copy SELECT (after ->> 'k')::int, (after ->> 'v')::int, \
                 after ->> 'note' FROM f WHERE seq = {}",
                c[0].unwrap()
            ))
            .unwrap();
        }
    }
    let table = rows(&mut s, "SELECT k, v, length(note) FROM t ORDER BY k");
    let copy = rows(&mut s, "SELECT k, v, length(note) FROM copy ORDER BY k");
    assert_eq!(copy, table, "{storage}: {} changes", changes.len());
    assert!(!changes.is_empty(), "{storage}");
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{storage}: {problems:#?}");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_feed_replayed_gives_its_table_on_every_storage() {
    for (i, storage) in STORAGES.iter().enumerate() {
        for seed in 0..3 {
            replayed(storage, 10 * i as u64 + seed);
        }
    }
}

/// A write committed after the snapshot of the transaction making a feed,
/// by a writer that saw no feed, is in neither: that transaction's commit
/// fails on it.
#[test]
fn a_write_between_the_feeds_snapshot_and_its_commit_fails_the_feed() {
    let kv = open(7);
    let mut a = Session::new(Arc::clone(&kv));
    let mut b = Session::new(Arc::clone(&kv));
    a.execute("CREATE TABLE t (k INT PRIMARY KEY, v INT); INSERT INTO t VALUES (1, 1)")
        .unwrap();
    a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM t"), [[Some(1)]]);
    b.execute("INSERT INTO t VALUES (2, 2)").unwrap();
    a.execute("CREATE CHANGEFEED f ON t").unwrap();
    let e = a.execute("COMMIT").unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    // Made again: its snapshot has the row.
    a.execute("CREATE CHANGEFEED f ON t").unwrap();
    b.execute("UPDATE t SET v = 3 WHERE k = 2").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM f"), [[Some(1)]]);
}
