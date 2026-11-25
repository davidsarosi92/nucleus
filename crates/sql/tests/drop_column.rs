//! A column dropped where it is stored (issue #18, step 2b): the rows keep
//! its place, readers pass over it, none rewritten — compared with the
//! same changes on a twin table whose rows are rewritten (a column
//! table's), through the read paths (a row at a time and vectorized,
//! indexes, values stored out of line), the writes that follow, columns
//! added after, a change of storage and a reopen; and a write committed
//! while the drop runs does not fail it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, parse};

/// The storages whose columns are dropped in place.
const IN_PLACE: &[&str] = &["", "WITH (engine = lsm)"];

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

/// The places of `table`'s dropped columns its rows still store.
fn gone(kv: &Arc<dyn Kv>, table: &str) -> Vec<usize> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::catalog::table(&*txn, table).unwrap().gone
}

/// Both tables, the same statement.
fn both(s: &mut Session, sql: &str) {
    for t in ["x", "r"] {
        s.execute(&sql.replace("TT", t))
            .unwrap_or_else(|e| panic!("{t}: {sql}: {e:?}"));
    }
}

/// `x` (columns dropped in place) and `r` (its rows rewritten) answer the
/// same to every query.
fn compare(s: &mut Session, queries: &[&str], what: &str) {
    for q in queries {
        let x = outcome(s, &q.replace("TT", "x")).unwrap();
        let r = outcome(s, &q.replace("TT", "r")).unwrap();
        assert_eq!(x, r, "{what}: {q}");
    }
}

fn setup(s: &mut Session, storage: &str) {
    s.execute("DROP TABLE IF EXISTS x; DROP TABLE IF EXISTS r")
        .unwrap();
    for (t, with) in [("x", storage), ("r", "WITH (storage = column)")] {
        s.execute(&format!(
            "CREATE TABLE {t} (id int PRIMARY KEY, a int, b text, c text, d numeric(8, 2)) {with}; \
             CREATE INDEX {t}_a ON {t} (a); CREATE INDEX {t}_ai ON {t} (a, id); \
             INSERT INTO {t} SELECT i, i % 13, \
               CASE WHEN i % 50 = 0 THEN repeat('long b ', 900) || i ELSE 'b' || i END, \
               CASE WHEN i % 40 = 0 THEN repeat('long c ', 900) || i ELSE 'c' || (i % 9) END, \
               i / 4.0 \
             FROM generate_series(1, 6000) i"
        ))
        .unwrap();
    }
}

#[test]
fn a_column_dropped_in_place_reads_and_writes_as_one_rewritten() {
    for storage in IN_PLACE {
        let p = Arc::new(SimPlatform::new(4));
        let kv = open_on(&p);
        let mut s = Session::new(Arc::clone(&kv));
        setup(&mut s, storage);
        let queries: &[&str] = &[
            "SELECT * FROM TT ORDER BY id",
            "SELECT count(*), sum(a), min(c), max(length(c)) FROM TT",
            "SELECT c, count(*), sum(d) FROM TT GROUP BY c ORDER BY c",
            "SELECT id, c FROM TT WHERE a = 7 AND id < 300 ORDER BY id",
            "SELECT id, length(c) FROM TT WHERE id % 40 = 0 ORDER BY id LIMIT 20",
        ];
        both(&mut s, "ALTER TABLE TT DROP COLUMN b");
        assert_eq!(gone(&kv, "x"), [2], "{storage}");
        assert_eq!(gone(&kv, "r"), Vec::<usize>::new(), "{storage}");
        compare(&mut s, queries, &format!("{storage} dropped"));
        check(
            &kv,
            &s,
            &format!("{storage} dropped (its index's entries removed)"),
        );
        // Writes after: updates (a long value made short, a short long),
        // deletes (a row whose dropped value was stored out of line),
        // inserts.
        both(
            &mut s,
            "UPDATE TT SET c = CASE WHEN id % 40 = 0 THEN 'short' ELSE repeat('now long ', 600) END \
             WHERE id % 7 = 0",
        );
        both(&mut s, "DELETE FROM TT WHERE id % 11 = 0 OR id % 50 = 0");
        both(
            &mut s,
            "INSERT INTO TT (id, a, c, d) SELECT i, i % 5, 'n' || i, 1.5 FROM generate_series(6001, 6100) i",
        );
        // A load's rows, made on its workers.
        let data: String = (6101..6200)
            .map(|i| format!("{i}\t{}\tl{i}\t2.5\n", i % 4))
            .collect();
        for t in ["x", "r"] {
            let stmt = parse(&format!("COPY {t} (id, a, c, d) FROM STDIN"))
                .unwrap()
                .pop()
                .unwrap();
            s.run_copy(&stmt, &[], &mut data.as_bytes()).unwrap();
        }
        compare(&mut s, queries, &format!("{storage} written"));
        check(&kv, &s, &format!("{storage} written"));
        // A column added (its value left for the rows), another dropped:
        // the places of the stored rows hold.
        both(&mut s, "ALTER TABLE TT ADD COLUMN e int DEFAULT 5");
        both(&mut s, "ALTER TABLE TT DROP COLUMN a");
        assert_eq!(gone(&kv, "x"), [1, 2], "{storage}");
        both(&mut s, "UPDATE TT SET e = 6 WHERE id % 3 = 0");
        both(&mut s, "ALTER TABLE TT ADD COLUMN f text DEFAULT 'f'");
        let after: &[&str] = &[
            "SELECT * FROM TT ORDER BY id",
            "SELECT e, f, count(*), sum(d) FROM TT GROUP BY e, f ORDER BY e, f",
            "SELECT id, c, e FROM TT WHERE e = 5 AND id BETWEEN 100 AND 130 ORDER BY id",
            "SELECT id, f FROM TT WHERE id = 6050",
        ];
        compare(&mut s, after, &format!("{storage} added and dropped"));
        check(&kv, &s, &format!("{storage} added and dropped"));
        // Reopened: the record keeps the places.
        drop(s);
        drop(kv);
        let kv = open_on(&p);
        let mut s = Session::new(Arc::clone(&kv));
        compare(&mut s, after, &format!("{storage} reopened"));
        // To a column table and back: its segments hold the columns.
        s.execute("ALTER TABLE x SET STORAGE column").unwrap();
        compare(&mut s, after, &format!("{storage} as columns"));
        // A column added there: its segments hold columns by number, so
        // the rows take its value written.
        both(&mut s, "ALTER TABLE TT ADD COLUMN g int DEFAULT 9");
        compare(&mut s, after, &format!("{storage} as columns, added"));
        s.execute("ALTER TABLE x SET STORAGE row").unwrap();
        compare(&mut s, after, &format!("{storage} as rows again"));
        check(&kv, &s, &format!("{storage} converted"));
        // A type change that rewrites the rows: not online (a row's hidden
        // value would follow the dropped places), in one transaction
        // writing them whole.
        assert_eq!(
            outcome(
                &mut s,
                "ALTER TABLE x ALTER COLUMN e TYPE numeric(6, 1) CONCURRENTLY"
            )
            .map_err(|e| e[..5].to_string()),
            Err("0A000".into()),
            "{storage}"
        );
        both(&mut s, "ALTER TABLE TT ALTER COLUMN e TYPE numeric(6, 1)");
        assert_eq!(gone(&kv, "x"), Vec::<usize>::new(), "{storage}");
        compare(&mut s, after, &format!("{storage} rewritten"));
        check(&kv, &s, &format!("{storage} rewritten"));
    }
}

/// A row's values stored out of line after a dropped column's place: an
/// update that gives one the other's value stores it (the old row's
/// values compared by their places).
#[test]
fn values_stored_out_of_line_after_a_dropped_place_are_updated() {
    for storage in IN_PLACE {
        let kv = open_on(&Arc::new(SimPlatform::new(7)));
        let mut s = Session::new(Arc::clone(&kv));
        s.execute(&format!(
            "CREATE TABLE y (id int PRIMARY KEY, b text, c text, e text) {storage}; \
             INSERT INTO y VALUES (1, 'b', repeat('first ', 2000), repeat('second ', 2000)); \
             ALTER TABLE y DROP COLUMN b; \
             UPDATE y SET c = e"
        ))
        .unwrap();
        assert_eq!(
            outcome(&mut s, "SELECT c = e, length(c) FROM y").unwrap(),
            [["t", "14000"]],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_write_committed_while_a_column_is_dropped_does_not_fail_it() {
    for storage in IN_PLACE {
        let kv = open_on(&Arc::new(SimPlatform::new(5)));
        let mut s = Session::new(Arc::clone(&kv));
        setup(&mut s, storage);
        let mut writer = Session::new(Arc::clone(&kv));
        for (t, dropped) in [("x", true), ("r", false)] {
            s.execute("BEGIN").unwrap();
            s.execute(&format!("ALTER TABLE {t} DROP COLUMN c"))
                .unwrap();
            writer
                .execute(&format!("UPDATE {t} SET a = a + 1 WHERE id = 77"))
                .unwrap();
            let r = outcome(&mut s, "COMMIT");
            if dropped {
                assert_eq!(r, Ok(Vec::new()), "{storage}: dropped in place");
            } else {
                // Its rows rewritten: read, a premise the write broke.
                assert!(r.as_ref().is_err_and(|e| e.starts_with("40001")), "{r:?}");
            }
        }
        assert_eq!(
            outcome(&mut s, "SELECT a FROM x WHERE id = 77").unwrap(),
            [["13"]], // 77 % 13, and the write's 1
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn columns_dropped_under_writes_that_do_not_pause() {
    for storage in IN_PLACE {
        let kv = open_on(&Arc::new(SimPlatform::new(6)));
        let mut s = Session::new(Arc::clone(&kv));
        s.execute(&format!(
            "CREATE TABLE w (id int PRIMARY KEY, n int, a text, b text, c text, d int) {storage}; \
             INSERT INTO w SELECT i, 0, 'a', 'b', 'c', i FROM generate_series(1, 3000) i"
        ))
        .unwrap();
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
                    w.execute(&format!("UPDATE w SET n = n + 1 WHERE id = {k}"))
                        .unwrap();
                }
                round
            })
        };
        for sql in [
            "ALTER TABLE w DROP COLUMN a",
            "ALTER TABLE w DROP COLUMN c",
            "ALTER TABLE w ADD COLUMN e int DEFAULT 1",
            "ALTER TABLE w DROP COLUMN b",
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
        assert_eq!(gone(&kv, "w"), [2, 3, 4], "{storage}");
        assert_eq!(
            outcome(
                &mut s,
                "SELECT count(*), sum(d), sum(e), min(n) >= 0 FROM w"
            )
            .unwrap(),
            [["3000", "4501500", "3000", "t"]],
            "{storage}"
        );
        assert_eq!(
            outcome(&mut s, "SELECT sum(n) FROM w").unwrap(),
            [[rounds.to_string()]],
            "{storage}: every write kept"
        );
        check(&kv, &s, storage);
    }
}
