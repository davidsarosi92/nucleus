//! A column added with a default whose value is one for every row
//! (issue #18, step 4): the rows stored before read it as theirs, none
//! rewritten — compared with the same column on a twin table whose rows
//! were rewritten (a volatile default's), through every read path: rows
//! and column segments, a row at a time and vectorized, indexes, the
//! changes that follow, a reopen.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

/// The storages a table's rows may have.
const STORAGES: &[&str] = &[
    "",
    "WITH (storage = column)",
    "WITH (storage = 'row,column')",
    "WITH (engine = lsm)",
];

fn open_on(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
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

/// Whether `table`'s added columns are values left for its rows (none
/// rewritten).
fn missing(kv: &Arc<dyn Kv>, table: &str) -> usize {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::catalog::table(&*txn, table)
        .unwrap()
        .missing
        .len()
}

const QUERIES: &[&str] = &[
    "SELECT * FROM TT ORDER BY id",
    "SELECT id, d, e FROM TT WHERE d = 7 ORDER BY id",
    "SELECT count(*), sum(d), min(e), max(f) FROM TT",
    "SELECT e, count(*) FROM TT GROUP BY e ORDER BY e",
    "SELECT id FROM TT WHERE f IS NULL ORDER BY id",
    "SELECT id, g FROM TT WHERE g > 1.5 AND id < 50 ORDER BY id",
    "SELECT count(*) FROM TT WHERE d = 7 AND e = 'new'",
    "SELECT id, d FROM TT WHERE id = 33",
    "SELECT id, d, e, g FROM TT WHERE b = 'b33'",
    "SELECT id, b, e FROM TT WHERE d = 7 AND e = 'old' AND id BETWEEN 40 AND 45 ORDER BY id",
];

/// `x` (the columns left for its rows) and `r` (its rows rewritten)
/// answer the same.
fn compare(s: &mut Session, what: &str) {
    for q in QUERIES {
        let r = outcome(s, &q.replace("TT", "r")).unwrap();
        let x = outcome(s, &q.replace("TT", "x")).unwrap();
        assert_eq!(x, r, "{what}: {q}");
    }
}

fn setup(s: &mut Session, storage: &str) {
    for t in ["r", "x"] {
        s.execute(&format!(
            "DROP TABLE IF EXISTS {t}; \
             CREATE TABLE {t} (id INT PRIMARY KEY, a INT, b TEXT) {storage}; \
             INSERT INTO {t} SELECT i, i % 7, 'b' || i FROM generate_series(1, 6000) i"
        ))
        .unwrap();
    }
    // The column copy made of the rows so far.
    let _ = s.execute("VACUUM");
}

/// The same columns added to both: on `r` by a volatile default (whose
/// value is made per row, so the rows are rewritten) that comes out the
/// same, on `x` by the constant.
fn add(s: &mut Session, x: &str, r: &str) {
    s.execute(&format!("ALTER TABLE x ADD COLUMN {x}")).unwrap();
    s.execute(&format!("ALTER TABLE r ADD COLUMN {r}")).unwrap();
}

#[test]
fn rows_stored_before_read_the_added_value() {
    let p = Arc::new(SimPlatform::new(1));
    let kv = open_on(&p);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        add(
            &mut s,
            "d INT NOT NULL DEFAULT 7",
            "d INT NOT NULL DEFAULT (7 + (random() * 0)::int)",
        );
        add(
            &mut s,
            "e VARCHAR(10) DEFAULT 'old'",
            "e VARCHAR(10) DEFAULT ('ol' || substr('d', 1 + (random() * 0)::int))",
        );
        add(
            &mut s,
            "f TEXT",
            "f TEXT DEFAULT (CASE WHEN random() < 2 THEN NULL END)",
        );
        add(
            &mut s,
            "g NUMERIC(6, 2) DEFAULT 2.5",
            "g NUMERIC(6, 2) DEFAULT (2.5 + (random() * 0)::int)",
        );
        assert_eq!(missing(&kv, "x"), 3, "{storage}");
        assert_eq!(missing(&kv, "r"), 0, "{storage}");
        compare(&mut s, &format!("{storage}: added"));
        // Writes after: new rows, old rows changed (their added columns
        // kept or set), deleted; the default changed (the rows before keep
        // theirs).
        for t in ["r", "x"] {
            s.execute(&format!(
                "INSERT INTO {t} (id, a, b) VALUES (7001, 1, 'n'), (7002, 2, 'n'); \
                 INSERT INTO {t} VALUES (7003, 3, 'n', 9, 'given', 'f', 3.25); \
                 UPDATE {t} SET a = a + 1 WHERE id % 5 = 0; \
                 UPDATE {t} SET e = 'new', d = 8 WHERE id % 11 = 0; \
                 DELETE FROM {t} WHERE id % 13 = 0; \
                 ALTER TABLE {t} ALTER COLUMN d SET DEFAULT 1"
            ))
            .unwrap();
        }
        s.execute("INSERT INTO r (id, a, b) VALUES (7004, 4, 'n')")
            .unwrap();
        s.execute("INSERT INTO x (id, a, b) VALUES (7004, 4, 'n')")
            .unwrap();
        compare(&mut s, &format!("{storage}: written"));
        // An index on an added column (its rows found through it), a type
        // change, a column dropped before it: positions move, values stay.
        for t in ["r", "x"] {
            s.execute(&format!(
                "CREATE INDEX {t}_d ON {t} (d, e); ALTER TABLE {t} ALTER COLUMN d TYPE bigint"
            ))
            .unwrap();
        }
        compare(&mut s, &format!("{storage}: indexed"));
        for t in ["r", "x"] {
            s.execute(&format!("ALTER TABLE {t} DROP COLUMN a"))
                .unwrap();
        }
        let _ = s.execute("VACUUM");
        compare(&mut s, &format!("{storage}: changed"));
        check(&kv, &s, storage);
    }
}

#[test]
fn the_added_values_survive_a_reopen() {
    let p = Arc::new(SimPlatform::new(2));
    {
        let kv = open_on(&p);
        let mut s = Session::new(kv);
        setup(&mut s, "");
        add(
            &mut s,
            "d INT NOT NULL DEFAULT 7",
            "d INT NOT NULL DEFAULT (7 + (random() * 0)::int)",
        );
        add(
            &mut s,
            "e VARCHAR(10) DEFAULT 'old'",
            "e VARCHAR(10) DEFAULT ('ol' || substr('d', 1 + (random() * 0)::int))",
        );
        add(&mut s, "f TEXT", "f TEXT");
        add(
            &mut s,
            "g NUMERIC(6, 2) DEFAULT 2.5",
            "g NUMERIC(6, 2) DEFAULT (2.5 + (random() * 0)::int)",
        );
    }
    let kv = open_on(&p);
    let mut s = Session::new(Arc::clone(&kv));
    assert_eq!(missing(&kv, "x"), 3);
    compare(&mut s, "reopened");
    check(&kv, &s, "reopened");
}

#[test]
fn a_default_not_one_for_every_row_rewrites_them() {
    let p = Arc::new(SimPlatform::new(3));
    let kv = open_on(&p);
    let mut s = Session::new(Arc::clone(&kv));
    setup(&mut s, "");
    for col in [
        "d DOUBLE PRECISION DEFAULT random()",
        "e TIMESTAMPTZ DEFAULT now()",
        "h INT DEFAULT 5 CHECK (h > 1)",
    ] {
        s.execute(&format!("ALTER TABLE x ADD COLUMN {col}"))
            .unwrap();
        assert_eq!(missing(&kv, "x"), 0, "{col}");
    }
    // Made per row: the rows' values differ.
    assert_eq!(
        outcome(&mut s, "SELECT count(DISTINCT d) > 2900 FROM x").unwrap(),
        [["t"]]
    );
    // A check the value breaks, a NOT NULL without one: refused.
    assert!(
        s.execute("ALTER TABLE x ADD COLUMN k INT DEFAULT 0 CHECK (k > 1)")
            .is_err()
    );
    assert!(
        s.execute("ALTER TABLE x ADD COLUMN k INT NOT NULL")
            .is_err()
    );
    check(&kv, &s, "after");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_column_added_under_writes_that_do_not_pause() {
    let p = Arc::new(SimPlatform::new(4));
    let kv = open_on(&p);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let writer = {
            let (kv, stop, done) = (Arc::clone(&kv), Arc::clone(&stop), Arc::clone(&done));
            std::thread::spawn(move || {
                let mut w = Session::new(kv);
                let mut round = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    round += 1;
                    done.store(round, Ordering::SeqCst);
                    let k = round * 7919 % 3000 + 1;
                    w.execute(&format!("UPDATE x SET a = a + 1 WHERE id = {k}"))
                        .unwrap();
                    w.execute(&format!(
                        "INSERT INTO x (id, a, b) VALUES ({}, 0, 'w')",
                        10_000 + round
                    ))
                    .unwrap();
                }
                round
            })
        };
        for (n, col) in ["d INT DEFAULT 7", "e TEXT DEFAULT 'old'", "f INT"]
            .iter()
            .enumerate()
        {
            // Between writes.
            let at = done.load(Ordering::SeqCst);
            while done.load(Ordering::SeqCst) < at + 20 {
                std::thread::yield_now();
            }
            // In one transaction: no row read, so no write fails it.
            let mut txn_s = Session::new(Arc::clone(&kv));
            let r = outcome(&mut txn_s, &format!("ALTER TABLE x ADD COLUMN {col}"));
            assert_eq!(r, Ok(Vec::new()), "{storage} {col}");
            assert_eq!(missing(&kv, "x"), (n + 1).min(2), "{storage} {col}");
        }
        stop.store(true, Ordering::SeqCst);
        let rounds = writer.join().unwrap();
        assert!(rounds > 10, "{storage}: {rounds} writes");
        let rows = outcome(&mut s, "SELECT count(*) FROM x").unwrap()[0][0].clone();
        assert_eq!(
            outcome(
                &mut s,
                "SELECT count(*) FROM x WHERE d = 7 AND e = 'old' AND f IS NULL"
            )
            .unwrap(),
            [[rows]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn no_row_is_a_premise_of_the_change() {
    let p = Arc::new(SimPlatform::new(5));
    let kv = open_on(&p);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    setup(&mut s, "");
    // A write committed under the change, made under the old definition:
    // its row reads the added value as the others do.
    s.execute("BEGIN; ALTER TABLE x ADD COLUMN d INT DEFAULT 7; ALTER TABLE x ADD COLUMN f TEXT")
        .unwrap();
    w.execute("UPDATE x SET a = 100 WHERE id = 1; INSERT INTO x VALUES (9001, 1, 'w')")
        .unwrap();
    s.execute("COMMIT").unwrap();
    assert_eq!(
        outcome(
            &mut s,
            "SELECT id, a, d, f FROM x WHERE id IN (1, 2, 9001) ORDER BY id"
        )
        .unwrap(),
        [
            ["1", "100", "7", "NULL"],
            ["2", "2", "7", "NULL"],
            ["9001", "1", "7", "NULL"]
        ]
    );
    // One whose rows are rewritten: they are a premise.
    s.execute("BEGIN; ALTER TABLE x ADD COLUMN g FLOAT DEFAULT random()")
        .unwrap();
    w.execute("UPDATE x SET a = 101 WHERE id = 1").unwrap();
    assert_eq!(
        s.execute("COMMIT")
            .map_err(|e| e.code.to_string())
            .unwrap_err(),
        "40001"
    );
    check(&kv, &s, "after");
}
