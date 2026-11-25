//! Constraints checked online (issue #18, step 3): `SET NOT NULL` and
//! `ADD CHECK` recorded first — writers obey them — then the rows of a
//! snapshot checked, then made to hold; `ADD UNIQUE` built as an index
//! online.  Each is what a statement run alone falls back to when its
//! transaction meets a write.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;

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

/// What is being validated on `table`.
fn pending(kv: &Arc<dyn Kv>, table: &str) -> usize {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::catalog::table(&*txn, table)
        .unwrap()
        .validating
        .len()
}

/// `sql` checked online, `between` run once it is recorded.
fn constrain_watched(
    kv: &Arc<dyn Kv>,
    s: &Session,
    sql: &str,
    between: &mut dyn FnMut() -> nucleus_sql::Result<()>,
) -> Result<bool, String> {
    let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
    nucleus_sql::validating::constrain_watched(kv, s.registry(), &stmt, between)
        .map_err(|e| e.code.to_string())
}

fn setup(s: &mut Session, storage: &str) {
    s.execute(&format!(
        "DROP TABLE IF EXISTS x; \
         CREATE TABLE x (id INT PRIMARY KEY, n INT, t TEXT) {storage}; \
         INSERT INTO x SELECT i, i % 100, 't' || i FROM generate_series(1, 6000) i"
    ))
    .unwrap();
}

#[test]
fn writers_obey_a_constraint_being_checked() {
    let kv = open(1);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let mut seen = Vec::new();
        let done = constrain_watched(
            &kv,
            &s,
            "ALTER TABLE x ALTER COLUMN t SET NOT NULL",
            &mut || {
                seen = vec![
                    code(&mut w, "INSERT INTO x VALUES (7001, 1, NULL)"),
                    code(&mut w, "UPDATE x SET t = NULL WHERE id = 3"),
                    code(&mut w, "INSERT INTO x VALUES (7002, 1, 'a')"),
                    code(&mut w, "ALTER TABLE x ADD COLUMN z INT"),
                    code(&mut w, "ALTER TABLE x ADD CONSTRAINT c CHECK (n >= 0)"),
                ];
                Ok(())
            },
        );
        assert_eq!(done, Ok(true), "{storage}");
        assert_eq!(
            seen,
            ["23502", "23502", "ok", "55006", "55006"],
            "{storage}"
        );
        assert_eq!(pending(&kv, "x"), 0);
        assert_eq!(
            code(&mut s, "INSERT INTO x VALUES (7003, 1, NULL)"),
            "23502"
        );
        let done = constrain_watched(
            &kv,
            &s,
            "ALTER TABLE x ADD CONSTRAINT c CHECK (n >= 0)",
            &mut || {
                seen = vec![
                    code(&mut w, "INSERT INTO x VALUES (7004, -1, 'a')"),
                    code(&mut w, "UPDATE x SET n = -5 WHERE id = 3"),
                    code(&mut w, "UPDATE x SET n = n + 1 WHERE id < 50"),
                    code(&mut w, "ALTER TABLE x RENAME TO y"),
                ];
                Ok(())
            },
        );
        assert_eq!(done, Ok(true), "{storage}");
        assert_eq!(seen, ["23514", "23514", "ok", "55006"], "{storage}");
        assert_eq!(code(&mut s, "UPDATE x SET n = -1 WHERE id = 9"), "23514");
        // Neither again: there already.
        assert_eq!(
            constrain_watched(
                &kv,
                &s,
                "ALTER TABLE x ALTER COLUMN t SET NOT NULL",
                &mut || Ok(())
            ),
            Ok(false)
        );
        assert_eq!(
            constrain_watched(
                &kv,
                &s,
                "ALTER TABLE x ADD CONSTRAINT c CHECK (n >= 0)",
                &mut || Ok(())
            ),
            Ok(false)
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_row_that_breaks_it_abandons_the_constraint() {
    let kv = open(2);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        s.execute("UPDATE x SET t = NULL, n = -3 WHERE id = 4000")
            .unwrap();
        for (sql, err) in [
            ("ALTER TABLE x ALTER COLUMN t SET NOT NULL", "23502"),
            ("ALTER TABLE x ADD CHECK (n >= 0)", "23514"),
        ] {
            let mut ran = false;
            let r = constrain_watched(&kv, &s, sql, &mut || {
                ran = true;
                Ok(())
            });
            assert!(ran, "{storage} {sql}");
            assert_eq!(r, Err(err.to_string()), "{storage} {sql}");
            assert_eq!(pending(&kv, "x"), 0, "{storage} {sql}");
        }
        // Nothing left: writers free.
        assert_eq!(code(&mut w, "INSERT INTO x VALUES (7001, -9, NULL)"), "ok");
        // A condition that does not bind: refused before anything is
        // recorded.
        let mut ran = false;
        let r = constrain_watched(&kv, &s, "ALTER TABLE x ADD CHECK (nope > 0)", &mut || {
            ran = true;
            Ok(())
        });
        assert_eq!(r, Err("42703".to_string()), "{storage}");
        assert!(!ran);
        check(&kv, &s, storage);
    }
}

#[test]
fn a_check_cut_short_is_taken_over_or_abandoned() {
    let kv = open(3);
    let mut s = Session::new(Arc::clone(&kv));
    let mut w = Session::new(Arc::clone(&kv));
    setup(&mut s, "");
    let other = Session::new(Arc::clone(&kv));
    let cut = |sql: &str| {
        let r = constrain_watched(&kv, &other, sql, &mut || {
            Err(nucleus_sql::Error::internal("cut short"))
        });
        assert!(r.is_err());
    };
    // Its statement again (in its own transaction): taken over.
    cut("ALTER TABLE x ALTER COLUMN t SET NOT NULL");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (7001, 1, NULL)"),
        "23502"
    );
    assert_eq!(code(&mut w, "ALTER TABLE x ADD CHECK (n >= 0)"), "55006");
    s.execute("ALTER TABLE x ALTER COLUMN t SET NOT NULL")
        .unwrap();
    assert_eq!(pending(&kv, "x"), 0);
    cut("ALTER TABLE x ADD CHECK (n >= 0)");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (7001, -1, 'a')"),
        "23514"
    );
    s.execute("ALTER TABLE x ADD CHECK (n >= 0)").unwrap();
    assert_eq!(pending(&kv, "x"), 0);
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (7001, -1, 'a')"),
        "23514"
    );
    // Online again: taken over.
    cut("ALTER TABLE x ADD CONSTRAINT d CHECK (n < 1000)");
    assert_eq!(
        constrain_watched(
            &kv,
            &s,
            "ALTER TABLE x ADD CONSTRAINT d CHECK (n < 1000)",
            &mut || Ok(())
        ),
        Ok(true)
    );
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (7002, 5000, 'a')"),
        "23514"
    );
    // The maintenance thread: abandoned.
    cut("ALTER TABLE x ALTER COLUMN n SET NOT NULL");
    assert_eq!(
        code(&mut w, "INSERT INTO x VALUES (7003, NULL, 'a')"),
        "23502"
    );
    assert_eq!(nucleus_sql::validating::abandon_all(&kv).unwrap(), 1);
    assert_eq!(code(&mut w, "INSERT INTO x VALUES (7003, NULL, 'a')"), "ok");
    check(&kv, &s, "after");
}

#[test]
fn a_unique_constraint_falls_back_to_an_index_built_online() {
    let kv = open(4);
    let mut s = Session::new(Arc::clone(&kv));
    for storage in STORAGES {
        setup(&mut s, storage);
        let reg = s.registry().clone();
        let instead = |sql: &str| {
            let stmt = nucleus_sql::parser::parse_one(sql).unwrap();
            nucleus_sql::online_index::instead(&kv, &reg, &stmt).map_err(|e| e.code.to_string())
        };
        assert_eq!(
            instead("ALTER TABLE x ADD CONSTRAINT x_u UNIQUE (n, id)"),
            Ok(Some("ALTER TABLE"))
        );
        assert_eq!(
            instead("ALTER TABLE x ADD UNIQUE (t)"),
            Ok(Some("ALTER TABLE"))
        );
        assert_eq!(
            instead("ALTER TABLE x ADD UNIQUE (n)"),
            Err("23505".to_string())
        );
        assert_eq!(
            code(&mut s, "INSERT INTO x VALUES (7001, 1, 't5')"),
            "23505"
        );
        assert_eq!(code(&mut s, "ALTER TABLE x DROP CONSTRAINT x_t_key"), "ok");
        assert_eq!(code(&mut s, "ALTER TABLE x DROP CONSTRAINT x_u"), "ok");
        assert_eq!(
            code(&mut s, "ALTER TABLE x DROP CONSTRAINT x_n_key"),
            "42704"
        );
        check(&kv, &s, storage);
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn constraints_added_under_writes_that_do_not_pause() {
    let kv = open(5);
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
                    let k = round * 7919 % 6000 + 1;
                    w.execute(&format!(
                        "UPDATE x SET n = (n + 1) % 100, t = 'w' || id WHERE id = {k}"
                    ))
                    .unwrap();
                }
                round
            })
        };
        for sql in [
            "ALTER TABLE x ALTER COLUMN t SET NOT NULL",
            "ALTER TABLE x ADD CONSTRAINT c CHECK (n >= 0 AND n < 100)",
            "ALTER TABLE x ADD CONSTRAINT u UNIQUE (t)",
            "ALTER TABLE x ALTER COLUMN t DROP NOT NULL",
            "ALTER TABLE x DROP CONSTRAINT c",
            "ALTER TABLE x DROP CONSTRAINT u",
        ] {
            let at = done.load(Ordering::SeqCst);
            while done.load(Ordering::SeqCst) < at + 5 {
                std::thread::yield_now();
            }
            s.execute(sql)
                .unwrap_or_else(|e| panic!("{storage} {sql}: {e:?}"));
        }
        stop.store(true, Ordering::SeqCst);
        let rounds = writer.join().unwrap();
        assert!(rounds > 10, "{storage}: {rounds} writes");
        assert_eq!(pending(&kv, "x"), 0, "{storage}");
        check(&kv, &s, storage);
    }
}
