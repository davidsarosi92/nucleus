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
