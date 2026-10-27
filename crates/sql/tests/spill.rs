//! Work past `work_mem` (PLAN.md §M14.11 L6): the same statements give the
//! same results with 64 kB as with the default, while the small limit
//! sends their sorts and materialised rows to scratch space.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn database() -> Database {
    Database::open_on(
        Arc::new(SimPlatform::new(11)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

/// Each query's rows, with `work_mem` as set by `set`.
fn results(db: &Database, set: &str, queries: &[&str]) -> Vec<Vec<Vec<Value>>> {
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(set).unwrap();
    queries.iter().map(|q| rows(&mut s, q)).collect()
}

#[test]
fn sorts_and_materialised_rows_spill_and_agree() {
    let db = database();
    {
        let kv: Arc<dyn Kv> = Arc::new(db.clone());
        let mut s = Session::new(kv);
        s.execute(
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
             CREATE TABLE t (id INT PRIMARY KEY, g INT, name TEXT, m mood, tags TEXT[]); \
             INSERT INTO t SELECT i, i % 97, 'name ' || (i * 7919 % 20000), \
               (ARRAY['sad', 'ok', 'happy'])[1 + i % 3]::mood, ARRAY['a' || (i % 5)] \
               FROM generate_series(1, 20000) i",
        )
        .unwrap();
    }
    let queries = [
        "SELECT id, name FROM t ORDER BY name DESC, id",
        "SELECT m, tags, id FROM t ORDER BY m, id DESC LIMIT 50",
        "SELECT g, id FROM t ORDER BY g NULLS FIRST, id OFFSET 19000",
        "WITH c AS MATERIALIZED (SELECT id, name FROM t WHERE g < 50) \
           SELECT (SELECT count(*) FROM c), (SELECT max(name) FROM c), (SELECT min(id) FROM c)",
        "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 30000) \
           SELECT count(*), sum(n) FROM r",
        "SELECT i FROM generate_series(1, 1000000000) i LIMIT 3",
        // Aggregation, DISTINCT and set operations (sorted, to compare).
        "SELECT name, count(*), sum(id), min(m), string_agg(id::text, ',' ORDER BY id) \
           FROM t GROUP BY name ORDER BY name",
        "SELECT g, count(DISTINCT name), array_agg(id ORDER BY id DESC) FROM t GROUP BY g ORDER BY g",
        "SELECT count(*), sum(id) FROM t",
        "SELECT DISTINCT name, m FROM t ORDER BY name, m",
        "SELECT count(*) FROM (SELECT DISTINCT name FROM t) d",
        "SELECT name FROM t WHERE g < 60 UNION SELECT name FROM t WHERE g > 30 ORDER BY 1",
        "SELECT name, m FROM t INTERSECT SELECT name, m FROM t WHERE id % 2 = 0 ORDER BY 1, 2",
        "SELECT name FROM t EXCEPT SELECT name FROM t WHERE g < 90 ORDER BY 1",
        "SELECT g FROM t INTERSECT ALL SELECT g FROM t WHERE id < 5000 ORDER BY 1",
        "SELECT name FROM t EXCEPT ALL SELECT name FROM t WHERE id % 3 = 0 ORDER BY 1",
    ];
    let before = db.scratch_files_made();
    let big = results(&db, "SET work_mem = '64MB'", &queries);
    assert_eq!(
        db.scratch_files_made(),
        before,
        "64 MB is enough: nothing spilled"
    );
    let small = results(&db, "SET work_mem = '64kB'", &queries);
    assert!(
        db.scratch_files_made() > before,
        "64 kB is not: work spilled"
    );
    for (q, (a, b)) in queries.iter().zip(big.iter().zip(&small)) {
        assert_eq!(a, b, "{q}");
    }
    assert_eq!(big[0].len(), 20_000);
    assert_eq!(
        big[4],
        vec![vec![Value::Int(30_000), Value::Int(450_015_000)]]
    );
}

#[test]
fn writes_collect_their_rows_past_work_mem() {
    let db = database();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute("SET work_mem = '64kB'").unwrap();
    s.execute(
        "CREATE TABLE src (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO src SELECT i, repeat('x', i % 40) FROM generate_series(1, 15000) i",
    )
    .unwrap();
    let before = db.scratch_files_made();
    // A write reading its own table collects first: INSERT … SELECT doubles.
    s.execute("INSERT INTO src SELECT id + 15000, v FROM src")
        .unwrap();
    assert!(db.scratch_files_made() > before);
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src"),
        vec![vec![Value::Int(30_000)]]
    );
    s.execute("UPDATE src SET v = v || '!' WHERE id % 2 = 0")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src WHERE v LIKE '%!'"),
        vec![vec![Value::Int(15_000)]]
    );
    let out = s
        .execute("UPDATE src SET id = id + 100000 WHERE id > 29990 RETURNING id")
        .unwrap();
    assert!(matches!(&out[0], Output::Rows { rows, .. } if rows.len() == 10));
    s.execute("CREATE INDEX ON src (v)").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src WHERE v = 'xxx!'"),
        rows(&mut s, "SELECT count(*) FROM src WHERE v || '' = 'xxx!'")
    );
    // (9996 below 29991, and 129993, 129996, 129999.)
    s.execute("DELETE FROM src WHERE id % 3 = 0").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src"),
        vec![vec![Value::Int(20_001)]]
    );
    // ANALYZE samples a large table, and counts it exactly.
    s.execute("ANALYZE src").unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT analyzed_rows FROM nc_catalog.tables WHERE name = 'src'"
        ),
        vec![vec![Value::Int(20_001)]]
    );
    // Past the sample: the count exact, distinct values estimated.
    s.execute(
        "CREATE TABLE wide (id INT PRIMARY KEY, g INT); \
         INSERT INTO wide SELECT i, i % 100 FROM generate_series(1, 40000) i; ANALYZE wide",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT column_name, rows, distinct_values FROM nc_catalog.statistics \
             WHERE table_name = 'wide' ORDER BY column_name"
        ),
        vec![
            vec![Value::Text("g".into()), Value::Int(40_000), Value::Int(100)],
            vec![
                Value::Text("id".into()),
                Value::Int(40_000),
                Value::Int(40_000)
            ],
        ]
    );
    let e = s.execute("SET work_mem = '1kB'").unwrap_err();
    assert_eq!(e.code, "22023");
}
