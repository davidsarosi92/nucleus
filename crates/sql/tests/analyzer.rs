//! The SQL analyzer (PLAN.md §M14.9) where slt cannot reach: `EXPLAIN
//! ANALYZE`'s times, which differ from run to run.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn open(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
}

fn lines(s: &mut Session, sql: &str) -> Vec<String> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Text(t) => t.clone(),
                v => panic!("not text: {v:?}"),
            })
            .collect(),
        o => panic!("not rows: {o:?}"),
    }
}

#[test]
fn explain_analyze_times_every_node() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::new(open(&p));
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, x INT)")
        .unwrap();
    s.execute("INSERT INTO t SELECT i, i % 3 FROM generate_series(1, 30) i")
        .unwrap();
    let out = lines(
        &mut s,
        "EXPLAIN ANALYZE SELECT x, count(*) FROM t GROUP BY x",
    );
    let (last, nodes) = out.split_last().unwrap();
    assert!(
        last.starts_with("Execution: 3 rows in ") && last.ends_with(" ms"),
        "{last}"
    );
    assert!(!nodes.is_empty());
    for n in nodes {
        assert!(n.contains(" (actual rows=") && n.contains(" time="), "{n}");
    }
}
