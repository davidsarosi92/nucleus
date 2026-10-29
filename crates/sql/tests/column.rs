//! Columnar storage (PLAN.md §M14.20 V7) against the row storage: the
//! same table in each, the same writes — loads that make several segments
//! and merge, updates, key changes, deletes, small writes left in the
//! delta store — and after each, the same rows and query results (a row and
//! a batch at a time, in parallel), and a consistent integrity check.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session};

fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            Some(_) => Ok(Vec::new()),
            None => Ok(Vec::new()),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

fn both(s: &mut Session, sql: &str) {
    let r = outcome(s, &sql.replace("TT", "r"));
    // (Errors name the table's constraints.)
    let c = outcome(s, &sql.replace("TT", "c")).map_err(|e| e.replace("\"c_", "\"r_"));
    assert_eq!(c, r, "{sql}");
}

const QUERIES: &[&str] = &[
    "SELECT * FROM TT ORDER BY id",
    "SELECT count(*), sum(x), min(x), max(x), avg(f), min(s), max(d) FROM TT",
    "SELECT g, count(*), sum(x), max(s) FROM TT GROUP BY g ORDER BY g",
    "SELECT id, x FROM TT WHERE x BETWEEN 100 AND 200 ORDER BY id",
    "SELECT id FROM TT WHERE s = 'name 7' ORDER BY id",
    "SELECT count(*) FROM TT WHERE s LIKE 'name 1%' AND ok",
    "SELECT id, d FROM TT WHERE d >= DATE '2026-06-01' ORDER BY id LIMIT 20",
    "SELECT * FROM TT WHERE id = 777",
    "SELECT * FROM TT WHERE id BETWEEN 30000 AND 30010",
    "SELECT id FROM TT WHERE x = 5 ORDER BY id",
    "SELECT count(*) FROM TT WHERE f IS NULL",
    "SELECT * FROM TT WHERE id > 1000000",
];

fn compare(s: &mut Session) {
    for q in QUERIES {
        both(s, q);
    }
}

#[test]
fn column_tables_agree_with_row_tables() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(4)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&db));
    for (t, storage) in [("r", "row"), ("c", "column")] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, g INT, x BIGINT, f FLOAT, s TEXT, ok BOOL, d DATE) \
             WITH (storage = {storage}); CREATE INDEX ON {t} (x)"
        ))
        .unwrap();
    }
    let rng = SplitMix64::new(9);
    let load = |from: i64, to: i64| {
        format!(
            "INSERT INTO TT SELECT i, i % 17, i * 7919 % 1000, \
             CASE WHEN i % 13 = 0 THEN NULL ELSE i * 0.5 END, 'name ' || (i % 50), i % 3 = 0, \
             DATE '2026-01-01' + i % 365 FROM generate_series({from}, {to}) i"
        )
    };
    let mut steps: Vec<String> = vec![
        load(1, 40000),     // past a segment's rows: two segments
        load(40001, 41000), // left in the delta store
        "UPDATE TT SET x = x + 1, s = s || '!' WHERE id % 100 = 0".into(),
        "DELETE FROM TT WHERE id % 37 = 0".into(),
        load(80001, 90000), // a merge, appended
        "UPDATE TT SET id = id + 200000 WHERE id BETWEEN 100 AND 150".into(),
        "DELETE FROM TT WHERE id BETWEEN 39990 AND 40500".into(),
        load(41001, 45000), // a merge among existing segments
    ];
    for _ in 0..6 {
        let id = rng.below(90000) as i64 + 1;
        steps.push(match rng.below(3) {
            0 => format!(
                "UPDATE TT SET g = g + 1 WHERE id BETWEEN {id} AND {}",
                id + 50
            ),
            1 => format!("DELETE FROM TT WHERE id BETWEEN {id} AND {}", id + 20),
            _ => format!(
                "INSERT INTO TT SELECT i, 1, 2, 3.0, 'late', true, DATE '2027-01-01' \
                 FROM generate_series({}, {}) i",
                1_000_000 + id,
                1_000_000 + id + 10
            ),
        });
    }
    // A write that fails leaves both as they were.
    steps.push("INSERT INTO TT VALUES (5, 1, 1, 1, 'dup', true, DATE '2026-01-01')".into());
    for step in &steps {
        both(&mut s, step);
        compare(&mut s);
    }
    let segments = outcome(
        &mut s,
        "SELECT count(*), sum(rows) > 45000, sum(deleted) > 0 FROM nc_catalog.segments \
         WHERE table_name = 'c'",
    )
    .unwrap();
    assert!(segments[0][0].parse::<i64>().unwrap() >= 2, "{segments:?}");
    assert_eq!(segments[0][1..], ["t", "t"]);
    for batch in ["0", "3"] {
        s.execute(&format!("SET batch_size = {batch}")).unwrap();
        compare(&mut s);
    }
    s.execute("SET batch_size = 1024").unwrap();
    s.execute("SET parallel_workers = 4").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    compare(&mut s);
    assert!(
        nucleus_sql::vector::parallel_runs() > before,
        "scanned in parallel"
    );
    let txn = db.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
        Vec::<String>::new()
    );
}

/// A delete of a segment's row and a merge that rewrites the segment, in
/// transactions at once, in either order: both commit (a marker is kept by
/// the row, so the merge does not conflict with it) and the delete holds.
#[test]
fn a_delete_and_a_merge_of_one_segment_both_hold() {
    for delete_first in [true, false] {
        let db: Arc<dyn Kv> = Arc::new(
            Database::open_on(
                Arc::new(SimPlatform::new(6)),
                Path::new("/db"),
                Options::default(),
            )
            .unwrap(),
        );
        let mut setup = Session::new(Arc::clone(&db));
        setup
            .execute(
                "CREATE TABLE c (id INT PRIMARY KEY, x INT) WITH (storage = column); \
                 INSERT INTO c SELECT i * 2, i FROM generate_series(1, 6000) i",
            )
            .unwrap();
        let mut merger = Session::new(Arc::clone(&db));
        let mut deleter = Session::new(Arc::clone(&db));
        merger.execute("BEGIN").unwrap();
        deleter.execute("BEGIN").unwrap();
        // Odd keys among the segment's even ones: the merge rewrites it.
        merger
            .execute("INSERT INTO c SELECT i * 2 + 1, i FROM generate_series(1, 5000) i")
            .unwrap();
        deleter.execute("DELETE FROM c WHERE id = 100").unwrap();
        let (first, second) = if delete_first {
            (&mut deleter, &mut merger)
        } else {
            (&mut merger, &mut deleter)
        };
        first.execute("COMMIT").unwrap();
        second.execute("COMMIT").unwrap();
        let left = outcome(
            &mut setup,
            "SELECT count(*), sum(CASE WHEN id = 100 THEN 1 ELSE 0 END) FROM c",
        )
        .unwrap();
        assert_eq!(left, [["10999", "0"]], "delete first: {delete_first}");
        let txn = db.begin(TxnOptions::read_only()).unwrap();
        assert_eq!(
            nucleus_sql::check::check(&*txn, setup.registry()).unwrap(),
            Vec::<String>::new()
        );
    }
}
