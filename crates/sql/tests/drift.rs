//! Drift and tests (PLAN.md §M14.27 S8): `ks_test`, `auc`, `psi`,
//! `js_divergence` and `chisq_test` give scipy's and scikit-learn's
//! answers (the expectations below are theirs, computed on the same
//! rows), and the same bits in every executor, with workers, and past
//! `work_mem` (both sides spilled, the walk reading one back from
//! scratch space).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn values(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn session(seed: u64) -> Session {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(seed)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    Session::new(kv)
}

const SETTINGS: [&str; 5] = [
    "SET batch_size = 0",
    "SET batch_size = 1024",
    "SET parallel_workers = 4",
    "SET work_mem = '64kB'",
    "SET parallel_workers = 1",
];

fn floats(v: &Value) -> Vec<f64> {
    match v {
        Value::Float(f) => vec![*f],
        Value::Array(a) => a.iter().flat_map(floats).collect(),
        other => panic!("{other:?}"),
    }
}

fn close(got: f64, want: f64, rel: f64) {
    assert!(
        (got - want).abs() <= rel * want.abs().max(1e-300),
        "{got} against {want}"
    );
}

#[test]
fn the_tests_give_scipys_answers_in_every_executor() {
    let mut s = session(21);
    s.execute(
        "CREATE TABLE c (i INT PRIMARY KEY, x DOUBLE PRECISION, s BOOLEAN);
         INSERT INTO c SELECT i, ((i::bigint * 2654435761) % 1000003)::float8 / 1000003
             + CASE WHEN i % 2 = 0 THEN 0.01 ELSE 0 END, i % 2 = 0
           FROM generate_series(1, 30000) AS t(i);
         CREATE TABLE d (i INT PRIMARY KEY, x DOUBLE PRECISION, s BOOLEAN) WITH (storage = column);
         INSERT INTO d SELECT i, ((i::bigint * 40503) % 65537)::float8
             + CASE WHEN i % 3 = 0 THEN 900 ELSE 0 END, i % 3 = 0
           FROM generate_series(1, 9000) AS t(i);
         CREATE TABLE f (i INT PRIMARY KEY, x INT, s BOOLEAN);
         INSERT INTO f SELECT i, (i * 7) % 5 + CASE WHEN i % 4 = 0 AND i % 5 = 0 THEN 1 ELSE 0 END,
             i % 4 = 0 FROM generate_series(1, 3000) AS t(i)",
    )
    .unwrap();
    let q = "SELECT (SELECT ks_test(x, s) FROM c), (SELECT auc(x, s) FROM c),
                    (SELECT psi(x, s) FROM c), (SELECT ks_test(x, s) FROM d),
                    (SELECT auc(x, s) FROM d), (SELECT psi(x, s, 20) FROM d),
                    (SELECT chisq_test(x, s) FROM f), (SELECT js_divergence(x, s) FROM f)";
    let mut first: Option<Vec<Value>> = None;
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        let row = values(&mut s, q).pop().unwrap();
        match &first {
            None => first = Some(row),
            Some(f) => assert_eq!(&row, f, "{setting}"),
        }
    }
    let r = first.unwrap();
    // c: 15,000 a side — past scipy's exact limit, so the asymptotic
    // p-value (Stephens' correction; scipy's kstwo gives 0.41339).
    let ks = floats(&r[0]);
    assert_eq!(ks[0], 0.0102);
    close(ks[1], 0.413_394_576_807_394_9, 0.005);
    close(floats(&r[1])[0], 0.510_030_653_333_333_3, 1e-15);
    close(floats(&r[2])[0], 0.002_019_382_365_735_145_4, 1e-12);
    // d: exact.
    let ks = floats(&r[3]);
    assert_eq!(ks[0], 0.015);
    close(ks[1], 0.757_164_241_570_831_1, 1e-12);
    close(floats(&r[4])[0], 0.514_037_25, 1e-15);
    assert!(floats(&r[5])[0] > 0.0);
    let chi = floats(&r[6]);
    close(chi[0], 240.0, 1e-12);
    close(chi[1], 9.277_854_169_203_6e-51, 1e-9);
    assert_eq!(chi[2], 4.0);
    close(floats(&r[7])[0], 0.124_511_249_783_653_1, 1e-12);
}

#[test]
fn groups_sides_and_nulls() {
    let mut s = session(22);
    s.execute(
        "CREATE TABLE t (i INT PRIMARY KEY, g INT, x DOUBLE PRECISION, s BOOLEAN);
         INSERT INTO t SELECT i, i % 3, CASE WHEN i % 11 = 0 THEN NULL ELSE i % 17 END,
             CASE WHEN i % 13 = 0 THEN NULL ELSE i % 2 = 0 END
           FROM generate_series(1, 6000) AS t(i)",
    )
    .unwrap();
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        // Per group, as the whole table restricted to the group.
        let by_group = values(
            &mut s,
            "SELECT g, ks_test(x, s), auc(x, s), psi(x, s), chisq_test(x, s), js_divergence(x, s)
               FROM t GROUP BY g ORDER BY g",
        );
        for row in &by_group {
            let Value::Int(g) = row[0] else { panic!() };
            let whole = values(
                &mut s,
                &format!(
                    "SELECT {g}, ks_test(x, s), auc(x, s), psi(x, s), chisq_test(x, s),
                            js_divergence(x, s) FROM t WHERE g = {g}"
                ),
            );
            assert_eq!(&whole[0], row, "{setting}");
        }
        // One side empty, or no rows: NULL.
        let none = values(
            &mut s,
            "SELECT ks_test(x, s), auc(x, s), psi(x, s), chisq_test(x, s), js_divergence(x, s)
               FROM t WHERE s",
        );
        assert!(none[0].iter().all(Value::is_null), "{none:?}");
    }
    // An integer side converts; another type is refused.
    s.execute("SELECT ks_test(x, i % 2) FROM t").unwrap();
    assert!(s.execute("SELECT ks_test(x, 'a') FROM t").is_err());
    assert!(s.execute("SELECT ks_test('a', s) FROM t").is_err());
    let e = s.execute("SELECT psi(x, s, 0) FROM t").unwrap_err();
    assert_eq!(e.code, "22023");
}
