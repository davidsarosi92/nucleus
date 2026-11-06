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

/// The estimate `EXPLAIN` shows for the scan of `sql`'s plan (it shows
/// one once the table was analyzed).
fn estimate(s: &mut Session, sql: &str) -> f64 {
    let plan: Vec<String> = values(s, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let line = plan
        .iter()
        .find(|l| l.contains("Scan") && l.contains("(rows="))
        .unwrap_or_else(|| panic!("{plan:?}"));
    let at = line.rfind("(rows=").unwrap() + 6;
    line[at..line[at..].find(')').unwrap() + at]
        .parse()
        .unwrap()
}

/// ANALYZE keeps a sample of rows, and a predicate the statistics cannot
/// describe — a distance below a radius, a pattern, a function of a
/// column — is estimated on it (PLAN.md §M14.12 V3).
#[test]
fn predicates_beyond_the_statistics_are_estimated_on_a_sample() {
    let mut s = session(14);
    s.execute(
        "CREATE TABLE e (id INT PRIMARY KEY, v VECTOR(3), w TEXT);
         INSERT INTO e SELECT i, ARRAY[(i % 100) / 100.0, (i % 37) / 37.0, (i % 11) / 11.0]::vector,
             'w' || (i % 50) FROM generate_series(1, 20000) AS t(i)",
    )
    .unwrap();
    let cases = [
        "SELECT id FROM e WHERE v <-> '[0,0,0]' < 0.5",
        "SELECT id FROM e WHERE v <=> '[1,0,0]' > 0.3",
        "SELECT id FROM e WHERE w LIKE 'w1%'",
        "SELECT id FROM e WHERE id % 7 = 0",
        "SELECT id FROM e WHERE length(w) = 2",
    ];
    s.execute("ANALYZE e").unwrap();
    for q in cases {
        let actual = values(&mut s, &format!("SELECT count(*) FROM ({q}) x"));
        let Value::Int(actual) = actual[0][0] else {
            panic!()
        };
        let est = estimate(&mut s, q);
        // A thousand rows sampled: within a few points of the truth.
        let error = (est - actual as f64).abs() / 20_000.0;
        assert!(error < 0.05, "{q}: {est} for {actual}");
    }
    // A parameter is not known when planning: the default, no error.
    let stmt =
        nucleus_sql::parser::parse_one("SELECT count(*) FROM e WHERE v <-> $1 < 0.5").unwrap();
    let out = s
        .run(&stmt, &[nucleus_sql::vectors::value(&[0.0, 0.0, 0.0])])
        .unwrap();
    let Output::Rows { rows, .. } = out else {
        panic!()
    };
    assert_eq!(rows[0][0], Value::Int(1599));
    // A column added since: the sample is of other columns, not used.
    s.execute("ALTER TABLE e ADD COLUMN z INT").unwrap();
    let est = estimate(&mut s, "SELECT id FROM e WHERE w LIKE 'w1%'");
    assert_eq!(est, 2000.0, "the default tenth");
    s.execute("ANALYZE e").unwrap();
    let est = estimate(&mut s, "SELECT id FROM e WHERE w LIKE 'w1%'");
    assert!((est - 4400.0).abs() < 1000.0, "{est}");
}
