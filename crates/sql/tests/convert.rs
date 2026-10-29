//! Storage per table and per column, converted online (PLAN.md §M14.20
//! V8): `ALTER TABLE … SET STORAGE` between `row`, `column` and `row,
//! column` (of every column or some), with writes between, beside and
//! during the conversions, and crashes in the middle of one — the rows,
//! the query results and the integrity check stay those of a plain row
//! table.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_sql::{Output, Session, TxnStatus};

fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            _ => Ok(Vec::new()),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message.replace("\"x_", "\"r_"))),
    }
}

const QUERIES: &[&str] = &[
    "SELECT * FROM TT ORDER BY id",
    "SELECT count(*), sum(g), sum(x), max(s) FROM TT",
    "SELECT g, count(*), sum(x) FROM TT GROUP BY g ORDER BY g",
    "SELECT x, count(*) FROM TT WHERE g < 5 GROUP BY x ORDER BY x LIMIT 10",
    "SELECT * FROM TT WHERE id = 1234",
    "SELECT id FROM TT WHERE s = 'v3' ORDER BY id LIMIT 7",
];

fn compare(s: &mut Session, what: &str) {
    for q in QUERIES {
        let r = outcome(s, &q.replace("TT", "r"));
        let x = outcome(s, &q.replace("TT", "x"));
        assert_eq!(x, r, "{what}: {q}");
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

fn storage(s: &mut Session) -> String {
    outcome(s, "SELECT storage FROM nc_catalog.tables WHERE name = 'x'").unwrap()[0][0].clone()
}

fn setup(s: &mut Session, rows: u64) {
    for t in ["r", "x"] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, g INT, x BIGINT, s TEXT); \
             CREATE INDEX ON {t} (x); \
             INSERT INTO {t} SELECT i, i % 11, i * 7 % 1000, 'v' || (i % 13) \
             FROM generate_series(1, {rows}) i"
        ))
        .unwrap();
    }
}

fn both(s: &mut Session, sql: &str) {
    for t in ["r", "x"] {
        s.execute(&sql.replace("TT", t))
            .unwrap_or_else(|e| panic!("{sql} on {t}: {e:?}"));
    }
}

#[test]
fn conversions_keep_the_rows() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(2)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&kv));
    setup(&mut s, 40000);
    let steps = [
        ("row, column", "row,column"),
        ("column", "column"),
        ("column (g, x)", "row,column"),
        ("row", "row"),
        ("column", "column"),
        ("row, column", "row,column"),
        ("row, column (s)", "row,column"),
        ("row", "row"),
    ];
    for (i, (to, name)) in steps.iter().enumerate() {
        s.execute(&format!("ALTER TABLE x SET STORAGE {to}"))
            .unwrap();
        assert_eq!(storage(&mut s), *name, "SET STORAGE {to}");
        compare(&mut s, &format!("after SET STORAGE {to}"));
        check(&kv, &s, &format!("after SET STORAGE {to}"));
        // Writes in each storage: some rows changed, some gone, some new.
        let k = i as i64 * 10_000;
        both(
            &mut s,
            &format!("UPDATE TT SET x = x + 1, s = s || '+' WHERE id % 97 = {i}"),
        );
        both(
            &mut s,
            &format!("DELETE FROM TT WHERE id BETWEEN {} AND {}", k + 10, k + 60),
        );
        both(
            &mut s,
            &format!(
                "INSERT INTO TT SELECT i, 1, 2, 'new' FROM generate_series({}, {}) i",
                100_000 + k,
                100_000 + k + 5000
            ),
        );
        compare(&mut s, &format!("after writes in {to}"));
        check(&kv, &s, &format!("after writes in {to}"));
    }
    // Not in a transaction block.
    s.execute("BEGIN").unwrap();
    assert_eq!(
        s.execute("ALTER TABLE x SET STORAGE column")
            .unwrap_err()
            .code,
        "25001"
    );
    s.execute("ROLLBACK").unwrap();
    assert!(
        s.execute("ALTER TABLE x SET STORAGE column (nope)")
            .is_err()
    );
}

#[test]
fn writes_go_on_during_a_conversion() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(3)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&kv));
    setup(&mut s, 100_000);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let converter = {
        let kv = Arc::clone(&kv);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut c = Session::new(kv);
            for to in ["column", "row, column", "row", "row, column (g)", "row"] {
                c.execute(&format!("ALTER TABLE x SET STORAGE {to}"))
                    .unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    // Writes to both tables in one transaction each, again when they
    // conflict, until the conversions end.
    let mut writes = 0;
    let mut i = 0u64;
    while (!done.load(std::sync::atomic::Ordering::SeqCst) && !converter.is_finished())
        || writes < 20
    {
        i += 1;
        let k = (i * 7919) % 100_000 + 1;
        let sql = match i % 3 {
            0 => format!(
                "UPDATE TT SET x = x + 1 WHERE id BETWEEN {k} AND {}",
                k + 20
            ),
            1 => format!("DELETE FROM TT WHERE id = {k}"),
            _ => format!("INSERT INTO TT VALUES ({}, 3, 4, 'w')", 200_000 + i),
        };
        let block = format!(
            "BEGIN; {}; {}; COMMIT",
            sql.replace("TT", "r"),
            sql.replace("TT", "x")
        );
        loop {
            match s.execute(&block) {
                Ok(_) => break,
                Err(e) => {
                    if s.status() != TxnStatus::Idle {
                        let _ = s.execute("ROLLBACK");
                    }
                    assert_eq!(e.code, "40001", "{block}: {e}");
                }
            }
        }
        writes += 1;
    }
    converter.join().unwrap();
    assert_eq!(storage(&mut s), "row");
    compare(&mut s, "after the conversions");
    check(&kv, &s, "after the conversions");
}

#[test]
fn a_conversion_cut_short_by_a_crash_finishes_when_run_again() {
    harness::run("convert_crash", 1, |seed| {
        let run = |p: &Arc<SimPlatform>| -> Option<Vec<Vec<String>>> {
            let db = Database::open_on(p.clone(), Path::new("/db"), Options::default()).ok()?;
            let kv: Arc<dyn Kv> = Arc::new(db);
            let mut s = Session::new(kv);
            if s.execute(
                "CREATE TABLE x (id INT PRIMARY KEY, g INT, s TEXT); \
                 INSERT INTO x SELECT i, i % 7, 'v' || i FROM generate_series(1, 40000) i",
            )
            .is_err()
            {
                return None;
            }
            let before = outcome(&mut s, "SELECT * FROM x ORDER BY id").ok()?;
            for to in ["column", "row, column", "row"] {
                if s.execute(&format!("ALTER TABLE x SET STORAGE {to}"))
                    .is_err()
                {
                    return Some(before);
                }
            }
            Some(before)
        };
        let dry = Arc::new(SimPlatform::new(seed));
        let rows = run(&dry).expect("the dry run");
        let total = dry.sim_fs().op_count();
        for n in (1..=total).step_by((total / 25).max(1) as usize) {
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
            // Run again: it finishes.
            s.execute("ALTER TABLE x SET STORAGE column").unwrap();
            assert_eq!(storage(&mut s), "column", "crash at op {n}");
            assert_eq!(
                outcome(&mut s, "SELECT * FROM x ORDER BY id").unwrap(),
                rows
            );
            check(&kv, &s, &format!("crash at op {n}, finished"));
        }
    });
}
