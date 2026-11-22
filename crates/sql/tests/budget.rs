//! A statement's budget over work the executor does not scan itself: an
//! index build reads every row of its table, and stops at the statement's
//! deadline (`57014`) as a scan does — on its workers too — leaving no
//! index behind (found by cancelling writes at random timeouts: `UPDATE`,
//! `DELETE` and `INSERT … SELECT` stopped within milliseconds, `CREATE
//! INDEX` ran to its end).

use std::sync::Arc;

use nucleus_kv::{Database, Kv};
use nucleus_sql::{Output, Session, Value};

fn count(s: &mut Session, sql: &str) -> Value {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { mut rows, .. } => rows.remove(0).remove(0),
        o => panic!("{o:?}"),
    }
}

#[test]
fn an_index_build_stops_at_the_statement_timeout() {
    let kv: Arc<dyn Kv> =
        Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO t SELECT i, 'v' || (i * 7919 % 200000) FROM generate_series(1, 200000) i",
    )
    .unwrap();
    for workers in ["1", "4"] {
        s.execute(&format!(
            "SET parallel_workers = {workers}; SET statement_timeout = '1ms'"
        ))
        .unwrap();
        let started = std::time::Instant::now();
        let e = s.execute("CREATE INDEX t_v ON t (v)").unwrap_err();
        assert_eq!(e.code, "57014", "{workers} workers: {e:?}");
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
        s.execute("SET statement_timeout = 0").unwrap();
        assert_eq!(
            count(
                &mut s,
                "SELECT count(*) FROM nc_catalog.indexes WHERE name = 't_v'"
            ),
            Value::Int(0),
            "a cancelled build left its index"
        );
    }
    // Without a deadline it builds, and answers as the table does.
    s.execute("CREATE INDEX t_v ON t (v)").unwrap();
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t WHERE v = 'v7919'"),
        count(&mut s, "SELECT count(*) FROM t WHERE v || '' = 'v7919'")
    );
}

/// Work that reads no rows stops at the deadline too: a nested loop's
/// pairs (its sides' product) and a sliding window's frames (found by
/// timing statements under a 100 ms `statement_timeout`: a nested loop of
/// 3,000 rows against 200,000 over a column table ran for 100 s — its left
/// side read in three batches, the only times its deadline was looked
/// at — and a `string_agg` over a 50,000-row frame never stopped).
#[test]
fn a_nested_loop_and_a_sliding_window_stop_at_the_statement_timeout() {
    let kv: Arc<dyn Kv> =
        Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, v TEXT, g TEXT); \
         INSERT INTO r SELECT i, 'v' || i, (i % 10)::text FROM generate_series(1, 40000) i; \
         CREATE TABLE c (id INT PRIMARY KEY, v TEXT, g TEXT) WITH (storage = column); \
         INSERT INTO c SELECT * FROM r",
    )
    .unwrap();
    for batch in ["0", "1024"] {
        for t in ["r", "c"] {
            for q in [
                // 1,500 × 40,000 pairs, none matching on a key.
                format!("SELECT count(*) FROM {t} a, {t} b WHERE a.v < b.v AND a.id <= 1500"),
                format!("SELECT count(*) FROM {t} a LEFT JOIN {t} b ON a.v < b.v AND a.id <= 1500"),
                // Each of 40,000 frames read anew (string_agg does not
                // combine partial states).
                format!(
                    "SELECT count(*) FROM (SELECT string_agg(g, '') OVER (ORDER BY id \
                     ROWS BETWEEN 4000 PRECEDING AND CURRENT ROW) FROM {t}) s"
                ),
            ] {
                s.execute(&format!(
                    "SET batch_size = {batch}; SET statement_timeout = '20ms'"
                ))
                .unwrap();
                let started = std::time::Instant::now();
                let e = s.execute(&q).map(|_| ()).unwrap_err();
                let took = started.elapsed();
                s.execute("SET statement_timeout = 0").unwrap();
                assert_eq!(e.code, "57014", "batch_size {batch}: {q}: {e:?}");
                assert!(
                    took.as_millis() < 1500,
                    "batch_size {batch}: {q}: stopped after {took:?}"
                );
            }
        }
    }
}
