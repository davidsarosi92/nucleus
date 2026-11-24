//! Indexes built online (issue #18): `CREATE INDEX CONCURRENTLY`, and a
//! `CREATE INDEX` whose build in one transaction meets a write — compared
//! with the build in one transaction on a twin table, with writes between
//! the build's parts, duplicates found or resolved meanwhile, other
//! changes of the table refused while it runs, crashes in the middle of
//! one, and writes that do not pause.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
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

fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            _ => Ok(Vec::new()),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

fn code(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(_) => "ok".into(),
        Err(e) => e.code.to_string(),
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

/// The indexes of `table` and whether each is ready.
fn indexes(s: &mut Session, table: &str) -> Vec<Vec<String>> {
    outcome(
        s,
        &format!(
            "SELECT name, is_ready FROM nc_catalog.indexes WHERE table_name = '{table}' \
             AND NOT is_primary ORDER BY name"
        ),
    )
    .unwrap()
}

/// `CREATE INDEX` built online, `between` run after each part of its
/// backfill commits.
fn build_watched(
    kv: &Arc<dyn Kv>,
    s: &Session,
    sql: &str,
    between: &mut dyn FnMut(&[u8]),
) -> Result<(), String> {
    let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
    nucleus_sql::online_index::create_watched(kv, s.registry(), &stmt, false, between)
        .map(|_| ())
        .map_err(|e| format!("{} {}", e.code, e.message))
}

const QUERIES: &[&str] = &[
    "SELECT id FROM TT WHERE v = 42 ORDER BY id",
    "SELECT id, v FROM TT WHERE v BETWEEN 100 AND 130 ORDER BY id",
    "SELECT id FROM TT WHERE g = 3 AND v < 200 ORDER BY id",
    "SELECT id FROM TT WHERE lower(s) = 'v7' ORDER BY id",
    "SELECT id FROM TT WHERE s = 'v3' AND id < 900 ORDER BY id",
    "SELECT count(*), sum(v) FROM TT WHERE v > 500",
];

/// The queries answer the same on `r` (its indexes built in one
/// transaction) and on `x` (built online); the integrity check compares
/// each index with the rows.
fn compare(s: &mut Session, what: &str) {
    for q in QUERIES {
        let r = outcome(s, &q.replace("TT", "r")).unwrap();
        let x = outcome(s, &q.replace("TT", "x")).unwrap();
        assert_eq!(x, r, "{what}: {q}");
    }
}

fn setup(s: &mut Session, storage: &str, rows: u64) {
    for t in ["r", "x"] {
        s.execute(&format!(
            "DROP TABLE IF EXISTS {t}; \
             CREATE TABLE {t} (id INT PRIMARY KEY, g INT, v INT, s TEXT, big TEXT) {storage}; \
             INSERT INTO {t} SELECT i, i % 11, i * 7 % 1000, 'v' || (i % 13), \
             CASE WHEN i % 500 = 0 THEN repeat('x', 6000 + i) END \
             FROM generate_series(1, {rows}) i"
        ))
        .unwrap();
    }
}

const DEFINITIONS: &[&str] = &["(v)", "(g, v)", "(lower(s))", "(s, id)", "(g, lower(s))"];

#[test]
fn concurrently_builds_as_one_transaction_does() {
    let kv = open(1);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage, 3000);
        for (n, def) in DEFINITIONS.iter().enumerate() {
            let unique = if n == 3 { "UNIQUE " } else { "" };
            s.execute(&format!("CREATE {unique}INDEX r_{n} ON r {def}"))
                .unwrap();
            s.execute(&format!(
                "CREATE {unique}INDEX CONCURRENTLY x_{n} ON x {def}"
            ))
            .unwrap_or_else(|e| panic!("{storage} {def}: {e:?}"));
        }
        let ready: Vec<Vec<String>> = (0..DEFINITIONS.len())
            .map(|n| vec![format!("x_{n}"), "t".to_string()])
            .collect();
        assert_eq!(indexes(&mut s, "x"), ready, "{storage}");
        compare(&mut s, storage);
        check(&kv, &s, storage);
        // A duplicate: refused as in one transaction, nothing left.
        assert_eq!(code(&mut s, "CREATE UNIQUE INDEX r_g ON r (g)"), "23505");
        assert_eq!(
            code(&mut s, "CREATE UNIQUE INDEX CONCURRENTLY x_g ON x (g)"),
            "23505",
            "{storage}"
        );
        assert_eq!(indexes(&mut s, "x"), ready, "{storage}");
        check(&kv, &s, &format!("{storage}: after a duplicate"));
        // Again with IF NOT EXISTS: there.
        s.execute("CREATE INDEX CONCURRENTLY IF NOT EXISTS x_0 ON x (g)")
            .unwrap();
        assert_eq!(
            code(&mut s, "CREATE INDEX CONCURRENTLY x_0 ON x (g)"),
            "42P07"
        );
    }
}

/// Writes after each part: rows before and after the part's end updated
/// (the indexed columns and the key), deleted, inserted.
fn writes(s: &mut Session, table: &str, round: u64) {
    let k = round * 397 % 19_000 + 1;
    for sql in [
        format!(
            "UPDATE {table} SET v = v + 1, s = 'w' || id WHERE id BETWEEN {k} AND {}",
            k + 40
        ),
        format!("UPDATE {table} SET g = g + 1 WHERE id = {}", 19_001 - k),
        format!("DELETE FROM {table} WHERE id = {}", k + 50),
        format!(
            "INSERT INTO {table} VALUES ({}, 3, 42, 'v7', NULL)",
            100_000 + round
        ),
        format!("UPDATE {table} SET id = id + 200000 WHERE id = {}", k + 60),
    ] {
        s.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

#[test]
fn writes_between_the_parts_are_kept() {
    let kv = open(2);
    let mut s = Session::new(Arc::clone(&kv));
    let mut round = 0;
    for storage in STORAGES {
        setup(&mut s, storage, 20_000);
        for (n, def) in DEFINITIONS.iter().enumerate() {
            let unique = if n == 3 { "UNIQUE " } else { "" };
            s.execute(&format!("CREATE {unique}INDEX r_{n} ON r {def}"))
                .unwrap();
            let mut w = Session::new(Arc::clone(&kv));
            let parts_before = round;
            build_watched(
                &kv,
                &s,
                &format!("CREATE {unique}INDEX CONCURRENTLY x_{n} ON x {def}"),
                &mut |_| {
                    round += 1;
                    writes(&mut w, "r", round);
                    writes(&mut w, "x", round);
                },
            )
            .unwrap_or_else(|e| panic!("{storage} {def}: {e}"));
            let parts = round - parts_before;
            assert!(parts > 1, "{storage} {def}: built in {parts} parts");
            compare(&mut s, &format!("{storage} {def}"));
            check(&kv, &s, &format!("{storage} {def}"));
        }
    }
}

#[test]
fn a_duplicate_resolved_during_a_unique_build_keeps_the_other_rows_entry() {
    let kv = open(3);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        s.execute(&format!(
            "DROP TABLE IF EXISTS x; \
             CREATE TABLE x (id INT PRIMARY KEY, u TEXT) {storage}; \
             INSERT INTO x SELECT i, CASE WHEN i = 4000 THEN 'u3' ELSE 'u' || i END \
             FROM generate_series(1, 5000) i"
        ))
        .unwrap();
        let mut w = Session::new(Arc::clone(&kv));
        let mut seen = Vec::new();
        build_watched(
            &kv,
            &s,
            "CREATE UNIQUE INDEX CONCURRENTLY x_u ON x (u)",
            &mut |_| {
                if seen.is_empty() {
                    // Row 3's entry written; its duplicate (4000) not
                    // yet reached: resolved, and a duplicate of row 5
                    // refused.
                    seen.push(code(&mut w, "UPDATE x SET u = 'fixed' WHERE id = 4000"));
                    seen.push(code(&mut w, "INSERT INTO x VALUES (6000, 'u5')"));
                    seen.push(code(&mut w, "UPDATE x SET u = 'u7' WHERE id = 4000"));
                    // A row the build has written the entry of, deleted
                    // (by its key alone, where the table has no index).
                    seen.push(code(&mut w, "DELETE FROM x WHERE id = 2"));
                }
            },
        )
        .unwrap_or_else(|e| panic!("{storage}: {e}"));
        assert_eq!(seen, ["ok", "23505", "23505", "ok"], "{storage}");
        assert_eq!(
            outcome(&mut s, "SELECT id FROM x WHERE u = 'u3'").unwrap(),
            [["3"]],
            "{storage}"
        );
        assert_eq!(
            outcome(&mut s, "SELECT id FROM x WHERE u = 'fixed'").unwrap(),
            [["4000"]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_duplicate_fails_the_build_and_leaves_nothing() {
    let kv = open(4);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        s.execute(&format!(
            "DROP TABLE IF EXISTS x; \
             CREATE TABLE x (id INT PRIMARY KEY, u TEXT) {storage}; \
             INSERT INTO x SELECT i, 'u' || i FROM generate_series(1, 5000) i"
        ))
        .unwrap();
        // A duplicate written meanwhile, of a row the build reaches later.
        let mut w = Session::new(Arc::clone(&kv));
        let mut first = true;
        let r = build_watched(
            &kv,
            &s,
            "CREATE UNIQUE INDEX CONCURRENTLY x_u ON x (u)",
            &mut |_| {
                if std::mem::take(&mut first) {
                    w.execute("INSERT INTO x VALUES (7000, 'u4500')").unwrap();
                }
            },
        );
        assert!(
            r.as_ref().is_err_and(|e| e.starts_with("23505")),
            "{storage}: {r:?}"
        );
        assert!(indexes(&mut s, "x").is_empty(), "{storage}");
        check(&kv, &s, storage);
        // The table takes writes and indexes as before.
        s.execute("INSERT INTO x VALUES (7001, 'u4500')").unwrap();
        s.execute("CREATE INDEX CONCURRENTLY x_u ON x (u)").unwrap();
        assert_eq!(
            outcome(&mut s, "SELECT id FROM x WHERE u = 'u4500' ORDER BY id").unwrap(),
            [["4500"], ["7000"], ["7001"]],
            "{storage}"
        );
        check(&kv, &s, &format!("{storage}: built again"));
    }
}

#[test]
fn other_changes_wait_for_a_build_and_drop_index_stops_it() {
    let kv = open(5);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE x (id INT PRIMARY KEY, g INT, v INT); \
         INSERT INTO x SELECT i, i % 7, i FROM generate_series(1, 20000) i",
    )
    .unwrap();
    let mut w = Session::new(Arc::clone(&kv));
    let mut seen = Vec::new();
    let r = build_watched(
        &kv,
        &s,
        "CREATE INDEX CONCURRENTLY x_v ON x (v)",
        &mut |_| {
            if !seen.is_empty() {
                return;
            }
            // Seen, not ready, and not used.
            seen.push(format!("{:?}", indexes(&mut w, "x")));
            for sql in [
                "ALTER TABLE x ADD COLUMN w INT",
                "CREATE INDEX x_g ON x (g)",
                "ALTER TABLE x RENAME TO y",
                "ALTER TABLE x ALTER COLUMN g TYPE BIGINT CONCURRENTLY",
                "CREATE INDEX CONCURRENTLY x_v ON x (g)",
                "SELECT count(*) FROM x WHERE v = 5",
                "DROP INDEX x_v",
            ] {
                seen.push(format!("{sql}: {}", code(&mut w, sql)));
            }
        },
    );
    assert_eq!(
        seen,
        [
            r#"[["x_v", "f"]]"#,
            "ALTER TABLE x ADD COLUMN w INT: 55006",
            "CREATE INDEX x_g ON x (g): 55006",
            "ALTER TABLE x RENAME TO y: 55006",
            "ALTER TABLE x ALTER COLUMN g TYPE BIGINT CONCURRENTLY: 0A000",
            "CREATE INDEX CONCURRENTLY x_v ON x (g): 42P07",
            "SELECT count(*) FROM x WHERE v = 5: ok",
            "DROP INDEX x_v: ok",
        ]
    );
    assert!(r.as_ref().is_err_and(|e| e.starts_with("55006")), "{r:?}");
    assert!(indexes(&mut s, "x").is_empty());
    check(&kv, &s, "after the drop");
    // What the online build refuses, and where.
    assert_eq!(
        code(&mut s, "BEGIN; CREATE INDEX CONCURRENTLY x_v ON x (v)"),
        "25001"
    );
    s.execute("ROLLBACK").unwrap();
    s.execute(
        "CREATE TABLE p (k INT, v INT, PRIMARY KEY (k)) PARTITION BY RANGE (k); \
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100)",
    )
    .unwrap();
    assert_eq!(
        code(&mut s, "CREATE INDEX CONCURRENTLY p_v ON p (v)"),
        "0A000"
    );
    // Built in one transaction where it cannot be built online.
    s.execute("CREATE INDEX p_v ON p (v)").unwrap();
    s.execute("CREATE INDEX CONCURRENTLY x_v ON x (v)").unwrap();
    s.execute("ALTER TABLE x ADD COLUMN w INT").unwrap();
    check(&kv, &s, "at the end");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn create_index_under_writes_that_do_not_pause() {
    let kv = open(6);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage, 60_000);
        s.execute("CREATE INDEX r_v ON r (v); CREATE UNIQUE INDEX r_s ON r (s, id)")
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (kv, stop) = (Arc::clone(&kv), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut w = Session::new(kv);
                let mut round = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    round += 1;
                    let k = round * 7919 % 60_000 + 1;
                    let sql = format!(
                        "BEGIN; UPDATE r SET v = (v + 1) % 1000, s = 'w' || id WHERE id = {k}; \
                         UPDATE x SET v = (v + 1) % 1000, s = 'w' || id WHERE id = {k}; COMMIT"
                    );
                    if w.execute(&sql).is_err() {
                        let _ = w.execute("ROLLBACK");
                    }
                }
                round
            })
        };
        // In one transaction each would meet a write; built online.
        let r = s.execute("CREATE INDEX x_v ON x (v)");
        let r2 = s.execute("CREATE UNIQUE INDEX x_s ON x (s, id)");
        stop.store(true, Ordering::SeqCst);
        let rounds = writer.join().unwrap();
        r.unwrap_or_else(|e| panic!("{storage}: {e:?}"));
        r2.unwrap_or_else(|e| panic!("{storage}: {e:?}"));
        assert!(rounds > 10, "{storage}: {rounds} writes");
        compare(&mut s, storage);
        check(&kv, &s, storage);
    }
}

#[test]
fn a_build_cut_short_by_a_crash_finishes() {
    harness::run("online_index_crash", 1, |seed| {
        let run = |p: &Arc<SimPlatform>| -> Option<Vec<Vec<String>>> {
            let db = Database::open_on(p.clone(), Path::new("/db"), Options::default()).ok()?;
            let kv: Arc<dyn Kv> = Arc::new(db);
            let mut s = Session::new(kv);
            if s.execute(
                "CREATE TABLE x (id INT PRIMARY KEY, g INT, v TEXT); \
                 INSERT INTO x SELECT i, i % 7, (i * 3)::text FROM generate_series(1, 20000) i",
            )
            .is_err()
            {
                return None;
            }
            let before = outcome(&mut s, "SELECT * FROM x ORDER BY id").ok()?;
            let _ = s.execute("CREATE UNIQUE INDEX CONCURRENTLY x_v ON x (v, g)");
            Some(before)
        };
        let dry = Arc::new(SimPlatform::new(seed));
        let rows = run(&dry).expect("the dry run");
        let total = dry.sim_fs().op_count();
        let mut middles = 0;
        for n in (1..=total).step_by((total / 30).max(1) as usize) {
            let p = Arc::new(SimPlatform::new(seed));
            p.sim_fs().set_faults(Faults {
                crash_mode: CrashMode::DropUnsynced,
                ..Faults::default()
            });
            p.sim_fs().crash_after_ops(n);
            let _ = run(&p);
            let db = Database::open_on(p.clone(), Path::new("/db"), Options::default())
                .unwrap_or_else(|e| panic!("crash at op {n}: reopen: {e}"));
            let kv: Arc<dyn Kv> = Arc::new(db);
            let mut s = Session::new(Arc::clone(&kv));
            let Ok(now) = outcome(&mut s, "SELECT * FROM x ORDER BY id") else {
                continue; // crashed before the table was there
            };
            if now.is_empty() {
                continue; // before the load committed
            }
            assert_eq!(now, rows, "crash at op {n}: the rows changed");
            check(&kv, &s, &format!("crash at op {n}"));
            // Cut short in the middle: written to while it waits, other
            // changes wait, the maintenance thread finishes it.
            if indexes(&mut s, "x") == [["x_v", "f"]] {
                middles += 1;
                s.execute("UPDATE x SET g = g + 7 WHERE id % 1000 = 1; DELETE FROM x WHERE id = 5")
                    .unwrap();
                assert_eq!(code(&mut s, "ALTER TABLE x ADD COLUMN w INT"), "55006");
                let done = nucleus_sql::online_index::resume_all(&kv, s.registry()).unwrap();
                assert_eq!(done, vec!["x_v".to_string()], "crash at op {n}");
                s.execute("ALTER TABLE x ADD COLUMN w INT").unwrap();
            }
            // Run again (or for the first time): there, ready.
            s.execute("CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS x_v ON x (v, g)")
                .unwrap();
            assert_eq!(indexes(&mut s, "x"), [["x_v", "t"]], "crash at op {n}");
            assert_eq!(
                outcome(&mut s, "SELECT id FROM x WHERE v = '30' AND g = 3").unwrap(),
                [["10"]],
                "crash at op {n}"
            );
            check(&kv, &s, &format!("crash at op {n}, finished"));
        }
        assert!(middles > 0, "no crash fell in the middle of the build");
    });
}

/// The keys under index `name`'s prefix (`nc_catalog.indexes.key_prefix`).
fn index_keys(kv: &Arc<dyn Kv>, prefix_hex: &str) -> usize {
    let prefix: Vec<u8> = (0..prefix_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&prefix_hex[i..i + 2], 16).unwrap())
        .collect();
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    txn.scan(
        std::ops::Bound::Included(&prefix[..]),
        std::ops::Bound::Excluded(&end[..]),
    )
    .unwrap()
    .count()
}

#[test]
fn a_dropped_index_meets_no_writer() {
    let kv = open(7);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        s.execute(&format!(
            "DROP TABLE IF EXISTS x; \
             CREATE TABLE x (id INT PRIMARY KEY, v INT) {storage}; \
             CREATE INDEX x_v ON x (v); CREATE UNIQUE INDEX x_u ON x (v, id); \
             INSERT INTO x SELECT i, i FROM generate_series(1, 3000) i"
        ))
        .unwrap();
        let prefixes = outcome(
            &mut s,
            "SELECT key_prefix FROM nc_catalog.indexes WHERE table_name = 'x' \
             AND NOT is_primary ORDER BY name",
        )
        .unwrap();
        let (v, u) = (prefixes[0][0].clone(), prefixes[1][0].clone());
        assert_eq!(index_keys(&kv, &v), 3000, "{storage}");
        // A drop whose snapshot is older than a write of the index's
        // keys: only the catalog changes, so it commits.
        let mut w = Session::new(Arc::clone(&kv));
        s.execute("BEGIN; DROP INDEX x_v; ALTER TABLE x DROP CONSTRAINT x_u")
            .unwrap();
        w.execute("UPDATE x SET v = v + 1 WHERE id <= 10").unwrap();
        assert_eq!(index_keys(&kv, &v), 3000, "{storage}: before the commit");
        s.execute("COMMIT").unwrap();
        // Its entries removed after it.
        assert_eq!(index_keys(&kv, &v), 0, "{storage}");
        assert_eq!(index_keys(&kv, &u), 0, "{storage}");
        check(&kv, &s, storage);
        // Rolled back: the index and its entries stay.
        s.execute("CREATE INDEX x_v ON x (v); BEGIN; DROP INDEX x_v; ROLLBACK")
            .unwrap();
        assert_eq!(indexes(&mut s, "x"), [["x_v", "t"]], "{storage}");
        assert_eq!(
            outcome(&mut s, "SELECT id FROM x WHERE v = 5").unwrap(),
            [["4"]],
            "{storage}"
        );
        check(&kv, &s, &format!("{storage}: rolled back"));
    }
}
