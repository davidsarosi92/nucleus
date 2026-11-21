//! A row table's scan when no column is read (PLAN.md §M14.33 R3): a
//! `count(*)` counts the live rows without their keys decoded or their
//! values read — the same answer as a count that reads a column, through
//! deletions, older versions, long values, a transaction's own writes,
//! snapshots and spilled writes; and `VACUUM ANALYZE` analyzes.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn kv(options: Options) -> Arc<dyn Kv> {
    let db = Database::open_on(Arc::new(SimPlatform::new(11)), Path::new("/db"), options).unwrap();
    Arc::new(db)
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        Output::Command(c) => panic!("{sql}: {c}"),
    }
}

/// `count(*)` (no column read) and `count(v)` (a column read; none null) of `t`,
/// asserted equal; the count.
fn count(s: &mut Session) -> i64 {
    let all = rows(s, "SELECT count(*) FROM t");
    let by_column = rows(s, "SELECT count(v) FROM t");
    assert_eq!(all, by_column);
    match all[0][0] {
        Value::Int(n) => n,
        ref v => panic!("{v:?}"),
    }
}

#[test]
fn a_count_without_columns_is_a_count_with_them() {
    let kv = kv(Options::default());
    let mut s = Session::new(kv.clone());
    s.execute(
        "CREATE TABLE t (id INT, k TEXT, v TEXT, PRIMARY KEY (k, id)); \
         INSERT INTO t SELECT i, 'k' || (i % 7), repeat('x', i % 50) \
         FROM generate_series(1, 5000) i",
    )
    .unwrap();
    assert_eq!(count(&mut s), 5000);
    // Long values: compressed in their cells, and in overflow pages.
    s.execute(
        "INSERT INTO t SELECT i, 'long', repeat(md5(i::text), 40) \
         FROM generate_series(1, 300) i; \
         INSERT INTO t SELECT i, 'longer', repeat(md5(i::text), 400) \
         FROM generate_series(1, 30) i",
    )
    .unwrap();
    assert_eq!(count(&mut s), 5330);
    // Deletions (tombstones) and older versions.
    s.execute("DELETE FROM t WHERE id % 3 = 0").unwrap();
    s.execute("UPDATE t SET v = 'changed' WHERE id % 5 = 0")
        .unwrap();
    s.execute("UPDATE t SET v = 'again' WHERE id % 10 = 0")
        .unwrap();
    let n = count(&mut s);
    assert_eq!(n, 5330 - 1666 - 100 - 10);
    // A transaction's own writes, not yet committed.
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (-1, 'own', 'x'), (-2, 'own', 'y')")
        .unwrap();
    s.execute("DELETE FROM t WHERE k = 'k1'").unwrap();
    let deleted = 5000 / 7 - 5000 / 21;
    let own = count(&mut s);
    assert!(own < n, "{own} {n}");
    assert_eq!(own, n + 2 - deleted as i64 - 1);
    s.execute("ROLLBACK").unwrap();
    assert_eq!(count(&mut s), n);
    // Another session's writes: not before they commit; a snapshot taken
    // before them keeps its count.
    let mut other = Session::new(kv.clone());
    other.execute("BEGIN").unwrap();
    other.execute("DELETE FROM t WHERE k = 'k2'").unwrap();
    assert_eq!(count(&mut s), n);
    s.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(count(&mut s), n);
    other.execute("COMMIT").unwrap();
    assert_eq!(count(&mut s), n);
    s.execute("COMMIT").unwrap();
    let after = count(&mut s);
    assert!(after < n);
    // Serializable reads are validated: counted as before.
    s.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(count(&mut s), after);
    s.execute("COMMIT").unwrap();
    // After a vacuum (older versions gone).
    s.execute("VACUUM").unwrap();
    assert_eq!(count(&mut s), after);
}

#[test]
fn a_count_reads_through_spilled_writes() {
    // Writes past a transaction's memory spill as intents: read through
    // their status until a vacuum makes them versions.
    let kv = kv(Options {
        txn_memory_bytes: 64 << 10,
        ..Options::default()
    });
    let mut s = Session::new(kv);
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    s.execute("INSERT INTO t SELECT i, repeat('v', 200) FROM generate_series(1, 20000) i")
        .unwrap();
    assert_eq!(count(&mut s), 20000);
    s.execute("DELETE FROM t WHERE id <= 5000").unwrap();
    assert_eq!(count(&mut s), 15000);
    s.execute("VACUUM").unwrap();
    assert_eq!(count(&mut s), 15000);
}

#[test]
fn vacuum_analyze_analyzes() {
    let mut s = Session::new(kv(Options::default()));
    s.execute(
        "CREATE TABLE a (id INT PRIMARY KEY); CREATE TABLE b (id INT PRIMARY KEY); \
         INSERT INTO a SELECT i FROM generate_series(1, 100) i; \
         INSERT INTO b SELECT i FROM generate_series(1, 50) i",
    )
    .unwrap();
    let analyzed = |s: &mut Session| {
        rows(
            s,
            "SELECT table_name, rows FROM nc_catalog.statistics ORDER BY table_name",
        )
    };
    s.execute("VACUUM a").unwrap();
    s.execute("VACUUM (ANALYZE false) a").unwrap();
    s.execute("VACUUM (VERBOSE) a").unwrap();
    assert!(analyzed(&mut s).is_empty());
    s.execute("VACUUM ANALYZE a").unwrap();
    assert_eq!(
        analyzed(&mut s),
        [[Value::Text("a".into()), Value::Int(100)]]
    );
    s.execute("INSERT INTO b SELECT i FROM generate_series(51, 60) i")
        .unwrap();
    s.execute("VACUUM (VERBOSE, ANALYZE) b (id)").unwrap();
    assert_eq!(
        analyzed(&mut s),
        [
            [Value::Text("a".into()), Value::Int(100)],
            [Value::Text("b".into()), Value::Int(60)]
        ]
    );
    s.execute("INSERT INTO a SELECT i FROM generate_series(101, 120) i")
        .unwrap();
    s.execute("VACUUM ANALYZE").unwrap();
    assert_eq!(
        analyzed(&mut s),
        [
            [Value::Text("a".into()), Value::Int(120)],
            [Value::Text("b".into()), Value::Int(60)]
        ]
    );
    assert_eq!(s.execute("VACUUM ANALYZE nope").unwrap_err().code, "42P01");
}

/// `ANALYZE` on the statement's workers (issue #4): the table's parts
/// sampled apart and merged, its columns worked out apart — the row count
/// exact and the estimates those of one sample of the whole.
#[test]
fn analyze_on_workers_estimates_as_on_one() {
    let stats = |workers: usize| {
        let mut s = Session::new(kv(Options::default()));
        s.execute(&format!("SET parallel_workers = {workers}"))
            .unwrap();
        s.execute(
            "CREATE TABLE t (id INT PRIMARY KEY, g INT, n INT, v TEXT); \
             INSERT INTO t SELECT i, i % 10, CASE WHEN i % 4 = 0 THEN NULL ELSE i END, \
             repeat('v', 30) FROM generate_series(1, 120000) i; ANALYZE t",
        )
        .unwrap();
        rows(
            &mut s,
            "SELECT column_name, rows, nulls, distinct_values FROM nc_catalog.statistics \
             ORDER BY column_name",
        )
    };
    let int = |v: &Value| match v {
        Value::Int(n) => *n,
        v => panic!("{v:?}"),
    };
    for workers in [1, 4] {
        let got = stats(workers);
        let col = |name: &str| {
            got.iter()
                .find(|r| r[0] == Value::Text(name.into()))
                .unwrap_or_else(|| panic!("{name} in {got:?}"))
        };
        for r in &got {
            assert_eq!(int(&r[1]), 120_000, "{workers} workers: {r:?}");
        }
        assert_eq!(int(&col("g")[3]), 10, "{workers} workers");
        assert_eq!(int(&col("id")[2]), 0, "{workers} workers");
        // A quarter null, from a sample of 30,000: within 2 %.
        let nulls = int(&col("n")[2]);
        assert!(
            (29_400..=30_600).contains(&nulls),
            "{workers} workers: {nulls}"
        );
        // Unique columns estimated unique (within 10 %).
        assert!(int(&col("id")[3]) >= 108_000, "{workers} workers: {got:?}");
    }
}
