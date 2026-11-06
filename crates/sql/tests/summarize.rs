//! `SUMMARIZE` (PLAN.md §M14.27 S8b): each column's figures are the
//! aggregates' own, the same in every executor, with workers and past
//! `work_mem`, in row and column storage.

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

#[test]
fn summarize_gives_the_aggregates_figures_everywhere() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(31)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, x DOUBLE PRECISION, w TEXT, d DATE, n NUMERIC(8, 2));
         INSERT INTO r SELECT i, CASE WHEN i % 9 = 0 THEN NULL ELSE i / 7.0 END, 'w' || (i % 40),
             DATE '2026-01-01' + i % 300, i / 4.0 FROM generate_series(1, 20000) AS t(i);
         CREATE TABLE c (id INT PRIMARY KEY, x DOUBLE PRECISION, w TEXT, d DATE, n NUMERIC(8, 2))
             WITH (storage = column);
         INSERT INTO c SELECT * FROM r",
    )
    .unwrap();
    let mut first: Option<Vec<Vec<Value>>> = None;
    for setting in [
        "SET batch_size = 0",
        "SET batch_size = 1024",
        "SET parallel_workers = 4",
        "SET work_mem = '64kB'",
        "SET parallel_workers = 1",
    ] {
        s.execute(setting).unwrap();
        for table in ["r", "c"] {
            let rows = values(&mut s, &format!("SUMMARIZE {table}"));
            match &first {
                None => first = Some(rows),
                Some(f) => assert_eq!(&rows, f, "{setting}, {table}"),
            }
        }
    }
    let rows = first.unwrap();
    let names: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
    assert_eq!(
        names,
        ["id", "x", "w", "d", "n"]
            .map(|n| Value::Text(n.into()))
            .iter()
            .collect::<Vec<_>>()
    );
    // x against its aggregates.
    let x = &rows[1];
    let direct = values(
        &mut s,
        "SELECT min(x)::text, max(x)::text, approx_count_distinct(x), avg(x), stddev(x),
                approx_percentile(x, ARRAY[0.25, 0.5, 0.75]), count(*) FROM r",
    )
    .pop()
    .unwrap();
    assert_eq!(&x[2..7], &direct[0..5]);
    let Value::Array(q) = &direct[5] else {
        panic!()
    };
    assert_eq!(&x[7..10], &q[..]);
    assert_eq!(x[10], direct[6]);
    // 2222 of 20,000 NULL: 11.11 %.
    assert_eq!(x[11], Value::Float(11.11));
    // A date has a range, no mean; text no quartiles.
    assert_eq!(rows[3][2], Value::Text("2026-01-01".into()));
    assert!(rows[3][5].is_null() && rows[2][7].is_null());
    assert_eq!(rows[2][4], Value::Int(40));
}
