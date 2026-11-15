//! A column's type changed online (PLAN.md §M14.18 O5): `ALTER TABLE …
//! ALTER COLUMN … TYPE … CONCURRENTLY` — compared with the transactional
//! form on a twin table, with writes during the change, values that do not
//! convert, and crashes in the middle of one: the rows, the query results
//! and the integrity check stay those of the transactional form.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_sql::{Output, Session, TxnStatus};

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

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

fn type_of(s: &mut Session, table: &str, column: &str) -> String {
    outcome(
        s,
        &format!(
            "SELECT type FROM nc_catalog.columns WHERE table_name = '{table}' AND name = '{column}'"
        ),
    )
    .unwrap()[0][0]
        .clone()
}

const QUERIES: &[&str] = &[
    "SELECT * FROM TT ORDER BY id",
    "SELECT count(*), max(v), min(v) FROM TT",
    "SELECT g, count(*), max(v) FROM TT GROUP BY g ORDER BY g",
    "SELECT id FROM TT WHERE v = 42 ORDER BY id",
    "SELECT id, v FROM TT WHERE v BETWEEN 100 AND 120 ORDER BY id",
    "SELECT id FROM TT WHERE s = 'v3' ORDER BY id LIMIT 7",
    "SELECT id, length(big) FROM TT WHERE big IS NOT NULL ORDER BY id",
];

fn compare(s: &mut Session, what: &str) {
    for q in QUERIES {
        let r = outcome(s, &q.replace("TT", "r"));
        let x = outcome(s, &q.replace("TT", "x"));
        if let (Ok(a), Ok(b)) = (&x, &r)
            && a != b
        {
            let only_x: Vec<_> = a.iter().filter(|row| !b.contains(row)).take(10).collect();
            let only_r: Vec<_> = b.iter().filter(|row| !a.contains(row)).take(10).collect();
            panic!(
                "{what}: {q}: {} rows against {}; only online: {only_x:?}; only transactional: {only_r:?}",
                a.len(),
                b.len()
            );
        }
        assert_eq!(x, r, "{what}: {q}");
    }
}

fn setup(s: &mut Session, rows: u64) {
    for t in ["r", "x"] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, g INT, v INT, s TEXT, big TEXT); \
             CREATE INDEX {t}_v ON {t} (v); \
             CREATE INDEX {t}_gv ON {t} (g, v); \
             CREATE UNIQUE INDEX {t}_s ON {t} (s, id); \
             INSERT INTO {t} SELECT i, i % 11, i * 7 % 1000, 'v' || (i % 13), \
             CASE WHEN i % 500 = 0 THEN repeat('x', 6000 + i) END \
             FROM generate_series(1, {rows}) i"
        ))
        .unwrap();
    }
}

/// Runs `ALTER COLUMN …` on `r` in one transaction and on `x` online.
fn alter_both(s: &mut Session, change: &str) {
    s.execute(&format!("ALTER TABLE r ALTER COLUMN {change}"))
        .unwrap_or_else(|e| panic!("{change} on r: {e:?}"));
    s.execute(&format!("ALTER TABLE x ALTER COLUMN {change} CONCURRENTLY"))
        .unwrap_or_else(|e| panic!("{change} CONCURRENTLY on x: {e:?}"));
}

#[test]
fn concurrently_converts_as_the_transactional_form() {
    let kv = open(2);
    let mut s = Session::new(Arc::clone(&kv));
    setup(&mut s, 20_000);
    let steps = [
        ("v TYPE BIGINT", "v", "bigint"),
        ("v TYPE TEXT", "v", "text"),
        ("v TYPE INT USING v::int", "v", "integer"),
        ("v TYPE NUMERIC(12, 2)", "v", "numeric(12,2)"),
        ("v TYPE BIGINT USING round(v)::bigint", "v", "bigint"),
        ("s TYPE VARCHAR(40)", "s", "varchar(40)"),
        ("big TYPE TEXT USING upper(big)", "big", "text"),
        ("big TYPE TEXT USING substr(big, 1, 3000)", "big", "text"),
    ];
    for (i, (change, column, ty)) in steps.iter().enumerate() {
        alter_both(&mut s, change);
        assert_eq!(type_of(&mut s, "x", column), *ty, "{change}");
        compare(&mut s, &format!("after {change}"));
        check(&kv, &s, &format!("after {change}"));
        // Writes between the changes.
        for t in ["r", "x"] {
            s.execute(&format!(
                "UPDATE {t} SET v = id % 300, s = s || '+' WHERE id % 97 = {i}; \
                 DELETE FROM {t} WHERE id BETWEEN {} AND {}; \
                 INSERT INTO {t} SELECT i, 1, i % 50, 'n' || i, NULL \
                 FROM generate_series({}, {}) i",
                i * 1000 + 10,
                i * 1000 + 40,
                100_000 + i * 1000,
                100_000 + i * 1000 + 300
            ))
            .unwrap();
        }
        compare(&mut s, &format!("writes after {change}"));
    }
    check(&kv, &s, "at the end");
    // Nothing left of the conversions: no record, the rows as long as the
    // table.
    assert_eq!(
        outcome(&mut s, "ALTER TABLE x ADD COLUMN w INT").map(|_| ()),
        Ok(())
    );
}

#[test]
fn checks_not_null_and_unique_hold_under_the_new_type() {
    let kv = open(4);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT NOT NULL CHECK (v <> '777'), w TEXT); \
         CREATE UNIQUE INDEX t_v ON t (v); \
         INSERT INTO t SELECT i, i::text, 'w' FROM generate_series(1, 5000) i \
         WHERE i <> 777; \
         INSERT INTO t VALUES (9001, '0042', 'w')",
    )
    .unwrap();
    let before = outcome(&mut s, "SELECT * FROM t ORDER BY id").unwrap();
    // '0042' and '42' are one integer: the unique index refuses it, the
    // table stays as it was.
    let e = s
        .execute("ALTER TABLE t ALTER COLUMN v TYPE INT USING v::int CONCURRENTLY")
        .unwrap_err();
    assert_eq!(e.code, "23505", "{e:?}");
    assert_eq!(type_of(&mut s, "t", "v"), "text");
    assert_eq!(
        outcome(&mut s, "SELECT * FROM t ORDER BY id").unwrap(),
        before
    );
    check(&kv, &s, "after a duplicate");
    // A value that does not convert.
    s.execute("DELETE FROM t WHERE id = 9001; INSERT INTO t VALUES (9002, 'x9', 'w')")
        .unwrap();
    let e = s
        .execute("ALTER TABLE t ALTER COLUMN v TYPE INT USING v::int CONCURRENTLY")
        .unwrap_err();
    assert_eq!(e.code, "22P02", "{e:?}");
    assert_eq!(type_of(&mut s, "t", "v"), "text");
    check(&kv, &s, "after a value that does not convert");
    // NULL where NOT NULL holds.
    let e = s
        .execute(
            "ALTER TABLE t ALTER COLUMN v TYPE INT \
             USING CASE WHEN v = 'x9' THEN NULL ELSE v::int END CONCURRENTLY",
        )
        .unwrap_err();
    assert_eq!(e.code, "23502", "{e:?}");
    check(&kv, &s, "after a NULL");
    // A check broken under the new type.
    s.execute("DELETE FROM t WHERE id = 9002").unwrap();
    let e = s
        .execute(
            "ALTER TABLE t ALTER COLUMN v TYPE INT \
             USING CASE WHEN v = '1' THEN 777 ELSE v::int END CONCURRENTLY",
        )
        .unwrap_err();
    assert_eq!(e.code, "23514", "{e:?}");
    assert_eq!(type_of(&mut s, "t", "v"), "text");
    check(&kv, &s, "after a check broken");
    // And then one that holds.
    s.execute("ALTER TABLE t ALTER COLUMN v TYPE INT USING v::int CONCURRENTLY")
        .unwrap();
    assert_eq!(type_of(&mut s, "t", "v"), "integer");
    assert_eq!(
        outcome(&mut s, "SELECT id FROM t WHERE v = 42").unwrap(),
        vec![vec!["42".to_string()]]
    );
    let e = s
        .execute("INSERT INTO t VALUES (9003, 42, 'w')")
        .unwrap_err();
    assert_eq!(e.code, "23505");
    let e = s
        .execute("INSERT INTO t VALUES (9004, 777, 'w')")
        .unwrap_err();
    assert_eq!(e.code, "23514");
    check(&kv, &s, "converted");
}

#[test]
fn what_the_online_form_refuses() {
    let kv = open(5);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE p (id INT PRIMARY KEY, v INT, t TIMESTAMPTZ); \
         CREATE TABLE c (id INT PRIMARY KEY, p INT REFERENCES p (id)); \
         CREATE INDEX p_e ON p ((v + 1)); \
         INSERT INTO p VALUES (1, 1, now())",
    )
    .unwrap();
    for (sql, code) in [
        (
            "ALTER TABLE p ALTER COLUMN id TYPE BIGINT CONCURRENTLY",
            "0A000",
        ),
        (
            "ALTER TABLE c ALTER COLUMN p TYPE BIGINT CONCURRENTLY",
            "0A000",
        ),
        (
            "ALTER TABLE p ALTER COLUMN v TYPE BIGINT CONCURRENTLY",
            "0A000",
        ),
        (
            "ALTER TABLE p ALTER COLUMN t TYPE TEXT CONCURRENTLY",
            "0A000",
        ),
        (
            "ALTER TABLE c ALTER COLUMN id TYPE BIGINT USING id + random()::int CONCURRENTLY",
            "0A000",
        ),
    ] {
        let e = s.execute(sql).unwrap_err();
        assert_eq!(e.code, code, "{sql}: {e:?}");
    }
    s.execute("BEGIN").unwrap();
    let e = s
        .execute("ALTER TABLE c ALTER COLUMN id TYPE BIGINT CONCURRENTLY")
        .unwrap_err();
    assert_eq!(e.code, "25001");
    s.execute("ROLLBACK").unwrap();
    // The transactional form does what it did.
    s.execute("ALTER TABLE p ALTER COLUMN v TYPE BIGINT")
        .unwrap();
    check(&kv, &s, "refusals");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn writes_go_on_during_a_type_change() {
    let kv = open(3);
    let mut s = Session::new(Arc::clone(&kv));
    setup(&mut s, 60_000);
    // r has the last type at once; x goes through them all, online.
    s.execute("ALTER TABLE r ALTER COLUMN v TYPE BIGINT")
        .unwrap();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let converter = {
        let kv = Arc::clone(&kv);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut c = Session::new(kv);
            for change in [
                "v TYPE BIGINT",
                "v TYPE TEXT",
                "v TYPE INT USING v::int",
                "v TYPE NUMERIC(14, 2)",
                "v TYPE BIGINT",
            ] {
                c.execute(&format!("ALTER TABLE x ALTER COLUMN {change} CONCURRENTLY"))
                    .unwrap_or_else(|e| panic!("{change}: {e:?}"));
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    // Writes to both tables in one transaction each — values every type of
    // the column takes — again when they conflict, until the changes end.
    let mut writes = 0;
    let mut i = 0u64;
    while (!done.load(std::sync::atomic::Ordering::SeqCst) && !converter.is_finished())
        || writes < 20
    {
        i += 1;
        let k = (i * 7919) % 60_000 + 1;
        let sql = match i % 4 {
            0 => format!(
                "UPDATE TT SET v = {}, s = 'u' || id WHERE id BETWEEN {k} AND {}",
                i % 900,
                k + 20
            ),
            1 => format!("DELETE FROM TT WHERE id = {k}"),
            2 => format!(
                "INSERT INTO TT VALUES ({}, 3, {}, 'w', NULL)",
                200_000 + i,
                i % 777
            ),
            _ => format!("UPDATE TT SET big = repeat('y', 7000) WHERE id = {k}"),
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
    assert_eq!(type_of(&mut s, "x", "v"), "bigint");
    check(&kv, &s, "after the changes");
    compare(&mut s, "after the changes");
}

#[test]
fn a_type_change_cut_short_by_a_crash_finishes() {
    harness::run("retype_crash", 1, |seed| {
        let run = |p: &Arc<SimPlatform>| -> Option<Vec<Vec<String>>> {
            let db = Database::open_on(p.clone(), Path::new("/db"), Options::default()).ok()?;
            let kv: Arc<dyn Kv> = Arc::new(db);
            let mut s = Session::new(kv);
            if s.execute(
                "CREATE TABLE x (id INT PRIMARY KEY, g INT, v TEXT); \
                 CREATE INDEX x_v ON x (v); \
                 INSERT INTO x SELECT i, i % 7, (i * 3)::text FROM generate_series(1, 20000) i",
            )
            .is_err()
            {
                return None;
            }
            let before = outcome(&mut s, "SELECT * FROM x ORDER BY id").ok()?;
            let _ =
                s.execute("ALTER TABLE x ALTER COLUMN v TYPE BIGINT USING v::bigint CONCURRENTLY");
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
            // Whichever the type, the values read the same.
            assert_eq!(now, rows, "crash at op {n}: the rows changed");
            check(&kv, &s, &format!("crash at op {n}"));
            let left =
                outcome(&mut s, "SELECT count(*) FROM x WHERE v = '42' OR v = '45'").unwrap();
            assert_eq!(left, vec![vec!["2".to_string()]], "crash at op {n}");
            // Cut short in the middle: other changes wait, the
            // maintenance thread's resumption finishes it.
            if let Err(e) = s.execute("ALTER TABLE x ADD COLUMN w INT") {
                assert_eq!(e.code, "55006", "crash at op {n}: {e:?}");
                middles += 1;
                let done = nucleus_sql::retype::resume_all(&kv, s.registry()).unwrap();
                assert_eq!(done, vec!["x".to_string()]);
                s.execute("ALTER TABLE x ADD COLUMN w INT").unwrap();
            }
            // Run again (or for the first time): it finishes.
            s.execute("ALTER TABLE x ALTER COLUMN v TYPE BIGINT USING v::bigint CONCURRENTLY")
                .unwrap();
            assert_eq!(type_of(&mut s, "x", "v"), "bigint", "crash at op {n}");
            assert_eq!(
                outcome(&mut s, "SELECT id, g, v FROM x ORDER BY id").unwrap(),
                rows,
                "crash at op {n}"
            );
            check(&kv, &s, &format!("crash at op {n}, finished"));
        }
        assert!(middles > 0, "no crash fell in the middle of the change");
    });
}
