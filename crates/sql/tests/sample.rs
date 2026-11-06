//! Sampling and splits (PLAN.md §M14.27 S7): a `TABLESAMPLE …
//! REPEATABLE` sample is the same rows in every executor and in row and
//! column storage, a smaller one within a larger; `hash_unit` splits the
//! same way; `reservoir_sample` is the same every run of a plan.

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

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!("{r:?}"),
        })
        .collect()
}

#[test]
fn a_repeatable_sample_is_the_same_rows_everywhere() {
    let mut s = session(11);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, x DOUBLE PRECISION);
         CREATE TABLE c (id INT PRIMARY KEY, x DOUBLE PRECISION) WITH (storage = column);
         CREATE TABLE k (a INT, b TEXT);
         INSERT INTO r SELECT i, i / 7.0 FROM generate_series(1, 30000) AS t(i);
         INSERT INTO c SELECT * FROM r;
         INSERT INTO k SELECT i % 50, 'b' || (i % 300) FROM generate_series(1, 6000) AS t(i)",
    )
    .unwrap();
    let mut first: Option<Vec<i64>> = None;
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        for table in ["r", "c"] {
            let ten = ints(&values(
                &mut s,
                &format!(
                    "SELECT id FROM {table} TABLESAMPLE BERNOULLI (10) REPEATABLE (42) ORDER BY id"
                ),
            ));
            match &first {
                None => first = Some(ten.clone()),
                Some(f) => assert_eq!(&ten, f, "{setting}, {table}"),
            }
            // SYSTEM samples the same rows; a smaller sample is within.
            let system = ints(&values(
                &mut s,
                &format!(
                    "SELECT id FROM {table} TABLESAMPLE SYSTEM (10) REPEATABLE (42) ORDER BY id"
                ),
            ));
            assert_eq!(system, ten);
            let one = ints(&values(
                &mut s,
                &format!(
                    "SELECT id FROM {table} TABLESAMPLE BERNOULLI (1) REPEATABLE (42) ORDER BY id"
                ),
            ));
            assert!(one.iter().all(|i| ten.binary_search(i).is_ok()));
            assert!((200..400).contains(&one.len()), "{}", one.len());
        }
        // Without a primary key: by all the columns, so a row's
        // duplicates (each (a, b) 20 times) are in or out together.
        let k = values(
            &mut s,
            "SELECT count(*), count(DISTINCT b) FROM k TABLESAMPLE BERNOULLI (50) REPEATABLE (3)",
        );
        let (Value::Int(rows), Value::Int(pairs)) = (&k[0][0], &k[0][1]) else {
            panic!()
        };
        assert_eq!(*rows, 20 * pairs, "{setting}");
        assert!((100..200).contains(pairs), "{pairs}");
    }
    let ten = first.unwrap();
    assert!((2700..3300).contains(&ten.len()), "{}", ten.len());
    // Another seed: another sample.
    let other = ints(&values(
        &mut s,
        "SELECT id FROM r TABLESAMPLE BERNOULLI (10) REPEATABLE (43) ORDER BY id",
    ));
    assert_ne!(other, ten);
}

#[test]
fn hash_unit_splits_the_same_way_and_nests() {
    let mut s = session(12);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY);
         INSERT INTO r SELECT i FROM generate_series(1, 20000) AS t(i)",
    )
    .unwrap();
    let mut first = None;
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        let train = values(
            &mut s,
            "SELECT count(*), sum(id) FROM r WHERE hash_unit(id) < 0.8",
        );
        match &first {
            None => first = Some(train),
            Some(f) => assert_eq!(&train, f, "{setting}"),
        }
    }
    let Value::Int(n) = first.unwrap()[0][0] else {
        panic!()
    };
    assert!((15_600..16_400).contains(&n), "{n}");
    // Train and test apart, together the whole.
    let parts = values(
        &mut s,
        "SELECT sum(CASE WHEN hash_unit(id, 7) < 0.8 THEN 1 ELSE 0 END),
                sum(CASE WHEN hash_unit(id, 7) >= 0.8 THEN 1 ELSE 0 END) FROM r",
    );
    let (Value::Int(a), Value::Int(b)) = (&parts[0][0], &parts[0][1]) else {
        panic!()
    };
    assert_eq!(a + b, 20_000);
}

#[test]
fn a_reservoir_sample_is_the_same_every_run() {
    let mut s = session(13);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, g INT, w TEXT);
         INSERT INTO r SELECT i, i % 3, CASE WHEN i % 10 = 0 THEN NULL ELSE 'w' || i END
           FROM generate_series(1, 30000) AS t(i)",
    )
    .unwrap();
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        let q = "SELECT g, reservoir_sample(w, 7), reservoir_sample(id, 100000, 5) FROM r GROUP BY g ORDER BY g";
        let a = values(&mut s, q);
        let b = values(&mut s, q);
        assert_eq!(a, b, "{setting}");
        for row in &a {
            let Value::Array(ws) = &row[1] else { panic!() };
            assert_eq!(ws.len(), 7);
            assert!(
                ws.iter().all(|w| matches!(w, Value::Text(_))),
                "NULLs left out"
            );
            // More asked than there are: all of them.
            let Value::Array(ids) = &row[2] else { panic!() };
            assert_eq!(ids.len(), 10_000);
        }
    }
}
