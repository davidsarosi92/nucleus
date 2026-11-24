//! Column type changes that keep every stored value (issue #18, step 2):
//! the same type changes nothing, a wider one only the catalog, and a
//! narrower one checks the rows — online, writers obeying the new type
//! meanwhile, when the check in the statement's transaction met a write.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
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

/// Column `column` of `table`'s type, as the catalog has it.
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

/// `sql` (a narrowing `ALTER COLUMN … TYPE`) checked online, `between`
/// run once the new type is recorded, before the rows are read.
fn alter_watched(
    kv: &Arc<dyn Kv>,
    s: &Session,
    sql: &str,
    between: &mut dyn FnMut(),
) -> Result<bool, String> {
    let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
    nucleus_sql::validating::alter_watched(kv, s.registry(), &stmt, between)
        .map_err(|e| format!("{} {}", e.code, e.message))
}

fn setup(s: &mut Session, storage: &str) {
    s.execute(&format!(
        "DROP TABLE IF EXISTS x; \
         CREATE TABLE x (id INT PRIMARY KEY, n BIGINT DEFAULT 7, t VARCHAR(30), u INT) {storage}; \
         CREATE INDEX x_n ON x (n); CREATE UNIQUE INDEX x_t ON x (t); \
         INSERT INTO x SELECT i, i % 1000, 'r' || i, i FROM generate_series(1, 5000) i"
    ))
    .unwrap();
}

#[test]
fn writers_obey_a_narrowing_being_checked() {
    let kv = open(1);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let mut seen = Vec::new();
        let done = alter_watched(
            &kv,
            &s,
            "ALTER TABLE x ALTER COLUMN n TYPE smallint",
            &mut || {
                seen = vec![
                    code(&mut w, "INSERT INTO x VALUES (6001, 70000, 'a', 1)"),
                    code(&mut w, "UPDATE x SET n = -40000 WHERE id = 3"),
                    code(&mut w, "INSERT INTO x VALUES (6002, 5, 'b', 1)"),
                    code(&mut w, "UPDATE x SET n = n + 1, u = 0 WHERE id < 100"),
                    code(&mut w, "ALTER TABLE x ADD COLUMN z INT"),
                    code(&mut w, "ALTER TABLE x ALTER COLUMN t TYPE text"),
                    code(&mut w, "ALTER TABLE x RENAME TO y"),
                ];
            },
        );
        assert_eq!(done, Ok(true), "{storage}");
        assert_eq!(
            seen,
            ["22003", "22003", "ok", "ok", "55006", "55006", "55006"],
            "{storage}"
        );
        assert_eq!(type_of(&mut s, "x", "n"), "smallint", "{storage}");
        assert_eq!(
            code(&mut s, "INSERT INTO x VALUES (6003, 40000, 'c', 1)"),
            "22003"
        );
        assert_eq!(
            outcome(&mut s, "SELECT count(*), sum(n) FROM x WHERE n = 6").unwrap(),
            [["5", "30"]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_value_that_does_not_fit_abandons_the_change() {
    let kv = open(2);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        s.execute("UPDATE x SET n = 70000, t = repeat('y', 25) WHERE id = 4000")
            .unwrap();
        for (sql, err) in [
            ("ALTER TABLE x ALTER COLUMN n TYPE bigint", None),
            ("ALTER TABLE x ALTER COLUMN u TYPE bigint", None),
            ("ALTER TABLE x ALTER COLUMN n TYPE smallint", Some("22003")),
            (
                "ALTER TABLE x ALTER COLUMN t TYPE varchar(20)",
                Some("22001"),
            ),
        ] {
            let mut ran = false;
            let r = alter_watched(&kv, &s, sql, &mut || ran = true);
            match err {
                // The same type, a wider one: no check — the statement's
                // to make.
                None => assert_eq!(r, Ok(false), "{storage} {sql}"),
                Some(err) => {
                    assert!(ran, "{storage} {sql}");
                    assert_eq!(r.map_err(|e| e[..5].to_string()), Err(err.to_string()));
                }
            }
        }
        // A default that does not fit: refused before any row is read.
        s.execute("ALTER TABLE x ALTER COLUMN u SET DEFAULT 99999")
            .unwrap();
        let mut ran = false;
        let r = alter_watched(
            &kv,
            &s,
            "ALTER TABLE x ALTER COLUMN u TYPE smallint",
            &mut || ran = true,
        );
        assert_eq!(r.map_err(|e| e[..5].to_string()), Err("22003".to_string()));
        assert!(!ran, "{storage}");
        // Nothing left: the types as they were, writers free.
        assert_eq!(type_of(&mut s, "x", "n"), "bigint", "{storage}");
        assert_eq!(
            code(&mut w, "INSERT INTO x VALUES (7000, 80000, 'd', 1)"),
            "ok"
        );
        assert_eq!(
            code(&mut w, "INSERT INTO x VALUES (7001, 1, repeat('z', 29), 1)"),
            "ok"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_change_cut_short_is_taken_over_or_abandoned() {
    let kv = open(3);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    setup(&mut s, "");
    let reg = s.registry().clone();
    let cut = |sql: &str| {
        let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = nucleus_sql::validating::alter_watched(&kv, &reg, &stmt, &mut || {
                panic!("cut short")
            });
        }));
        assert!(r.is_err());
    };
    // Its statement again: taken over, finished — online, or in the
    // statement's transaction.
    cut("ALTER TABLE x ALTER COLUMN n TYPE smallint");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (6001, 70000, 'a', 1)"),
        "22003"
    );
    assert_eq!(
        code(&mut w, "ALTER TABLE x ALTER COLUMN n TYPE integer"),
        "55006"
    );
    assert_eq!(
        alter_watched(
            &kv,
            &s,
            "ALTER TABLE x ALTER COLUMN n TYPE smallint",
            &mut || ()
        ),
        Ok(true)
    );
    assert_eq!(type_of(&mut s, "x", "n"), "smallint");
    cut("ALTER TABLE x ALTER COLUMN u TYPE smallint");
    s.execute("ALTER TABLE x ALTER COLUMN u TYPE smallint")
        .unwrap();
    assert_eq!(type_of(&mut s, "x", "u"), "smallint");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (6001, 1, 'a', 70000)"),
        "22003"
    );
    // The maintenance thread: abandoned.
    cut("ALTER TABLE x ALTER COLUMN t TYPE varchar(10)");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (6001, 1, repeat('a', 20), 1)"),
        "22001"
    );
    assert_eq!(nucleus_sql::validating::abandon_all(&kv).unwrap(), 1);
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (6001, 1, repeat('a', 20), 1)"),
        "ok"
    );
    assert_eq!(nucleus_sql::validating::abandon_all(&kv).unwrap(), 0);
    check(&kv, &s, "after");
}

#[test]
fn in_a_transaction_block_the_rows_are_checked_there() {
    let kv = open(4);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    setup(&mut s, "");
    // The rows a premise of the commit: a value written meanwhile under
    // the old type fails it.
    s.execute("BEGIN; ALTER TABLE x ALTER COLUMN n TYPE smallint")
        .unwrap();
    w.execute("UPDATE x SET n = 70000 WHERE id = 1").unwrap();
    assert_eq!(code(&mut s, "COMMIT"), "40001");
    assert_eq!(type_of(&mut s, "x", "n"), "bigint");
    w.execute("UPDATE x SET n = 1 WHERE id = 1").unwrap();
    s.execute("UPDATE x SET n = 70000 WHERE id = 10").unwrap();
    s.execute(
        "BEGIN; ALTER TABLE x ALTER COLUMN u TYPE bigint; ALTER TABLE x ALTER COLUMN t TYPE text",
    )
    .unwrap();
    assert_eq!(
        code(&mut s, "ALTER TABLE x ALTER COLUMN n TYPE smallint"),
        "22003"
    );
    s.execute("ROLLBACK").unwrap();
    // A default that does not fit the narrower type.
    assert_eq!(
        code(
            &mut s,
            "BEGIN; ALTER TABLE x ALTER COLUMN u SET DEFAULT 99999; \
             ALTER TABLE x ALTER COLUMN u TYPE smallint"
        ),
        "22003"
    );
    s.execute("ROLLBACK").unwrap();
    s.execute(
        "BEGIN; ALTER TABLE x ALTER COLUMN u TYPE bigint; ALTER TABLE x ALTER COLUMN t TYPE text; \
         ALTER TABLE x ALTER COLUMN n TYPE integer; COMMIT",
    )
    .unwrap();
    assert_eq!(type_of(&mut s, "x", "u"), "bigint");
    assert_eq!(type_of(&mut s, "x", "t"), "text");
    assert_eq!(type_of(&mut s, "x", "n"), "integer");
    assert_eq!(
        outcome(&mut s, "SELECT id FROM x WHERE n = 70000").unwrap(),
        [["10"]]
    );
    check(&kv, &s, "after");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn type_changes_under_writes_that_do_not_pause() {
    let kv = open(5);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (kv, stop) = (Arc::clone(&kv), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut w = Session::new(kv);
                let mut round = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    round += 1;
                    let k = round * 7919 % 5000 + 1;
                    let sql = format!(
                        "UPDATE x SET n = (n + 1) % 1000, t = 'w' || id, u = u + 1 WHERE id = {k}"
                    );
                    w.execute(&sql).unwrap();
                }
                round
            })
        };
        for (sql, ty) in [
            ("ALTER TABLE x ALTER COLUMN n TYPE integer", "integer"),
            ("ALTER TABLE x ALTER COLUMN n TYPE integer", "integer"),
            ("ALTER TABLE x ALTER COLUMN n TYPE smallint", "smallint"),
            ("ALTER TABLE x ALTER COLUMN n TYPE bigint", "bigint"),
            ("ALTER TABLE x ALTER COLUMN t TYPE text", "text"),
            ("ALTER TABLE x ALTER COLUMN t TYPE varchar(12)", "varchar"),
        ] {
            s.execute(sql)
                .unwrap_or_else(|e| panic!("{storage} {sql}: {e:?}"));
            let got = type_of(&mut s, "x", sql.split(' ').nth(5).unwrap());
            assert!(got.starts_with(ty), "{storage} {sql}: {got}");
        }
        stop.store(true, Ordering::SeqCst);
        let rounds = writer.join().unwrap();
        assert!(rounds > 10, "{storage}: {rounds} writes");
        assert_eq!(
            outcome(&mut s, "SELECT count(*) FROM x WHERE n >= 0 AND n < 1000").unwrap(),
            [["5000"]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_rewrite_that_meets_writes_is_converted_online() {
    let kv = open(6);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let reg = s.registry().clone();
        let fall_back = |sql: &str| {
            let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
            nucleus_sql::retype::fall_back(&kv, &reg, &stmt).map_err(|e| e.code.to_string())
        };
        // Values that stay: not a conversion (the statement's, or a
        // check's).
        assert_eq!(
            fall_back("ALTER TABLE x ALTER COLUMN u TYPE bigint"),
            Ok(false)
        );
        assert_eq!(
            fall_back("ALTER TABLE x ALTER COLUMN u TYPE smallint"),
            Ok(false)
        );
        // A rewrite: converted online where the rows are stored alone.
        let row = storage.is_empty() || storage.contains("lsm");
        assert_eq!(
            fall_back("ALTER TABLE x ALTER COLUMN u TYPE numeric"),
            Ok(row),
            "{storage}"
        );
        let ty = if row { "numeric" } else { "integer" };
        assert_eq!(type_of(&mut s, "x", "u"), ty, "{storage}");
        assert_eq!(
            outcome(&mut s, "SELECT sum(u) FROM x").unwrap(),
            [["12502500"]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}
