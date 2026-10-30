//! Partitioning (PLAN.md §M14.20 V9) against a plain table: the same writes
//! — routed inserts, updates that move rows between partitions, deletes —
//! leave the same rows, every query agrees, partitions are left out when a
//! parameter rules them out, and the integrity check holds.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session, Value};

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        _ => Vec::new(),
    }
}

fn with_params(s: &mut Session, sql: &str, params: &[Value]) -> Vec<Vec<String>> {
    let stmt = nucleus_sql::parser::parse(sql).unwrap().remove(0);
    match s.run(&stmt, params).unwrap() {
        Output::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn partitioned_tables_agree_with_plain_ones() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE plain (k INT, id INT, v TEXT, PRIMARY KEY (k, id)); \
         CREATE TABLE p (k INT, id INT, v TEXT, PRIMARY KEY (k, id)) PARTITION BY RANGE (k); \
         CREATE TABLE p0 PARTITION OF p FOR VALUES FROM (MINVALUE) TO (100); \
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (100) TO (200) WITH (storage = column); \
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (200) TO (300) WITH (storage = 'row,column'); \
         CREATE TABLE pd PARTITION OF p DEFAULT; \
         CREATE INDEX p_v ON p (v); CREATE INDEX plain_v ON plain (v)",
    )
    .unwrap();
    let rng = SplitMix64::new(3);
    for step in 0..40 {
        let k = rng.below(400) as i64 - 50;
        let sql = match rng.below(4) {
            0 | 1 => format!(
                "INSERT INTO TT SELECT {k} + i % 7, {step} * 1000 + i, 'v' || (i % 13) \
                 FROM generate_series(1, {}) i",
                1 + rng.below(300)
            ),
            2 => format!(
                "UPDATE TT SET k = k + {}, v = v || '!' WHERE id % 11 = {}",
                rng.below(150),
                rng.below(11)
            ),
            _ => format!("DELETE FROM TT WHERE k BETWEEN {k} AND {}", k + 20),
        };
        for t in ["plain", "p"] {
            s.execute(&sql.replace("TT", t))
                .unwrap_or_else(|e| panic!("{sql} on {t}: {e:?}"));
        }
    }
    for q in [
        "SELECT * FROM TT ORDER BY k, id",
        "SELECT k / 50, count(*), max(v) FROM TT GROUP BY 1 ORDER BY 1",
        "SELECT count(*) FROM TT WHERE k BETWEEN 120 AND 180",
        "SELECT id FROM TT WHERE v = 'v3!' ORDER BY id",
        "SELECT count(*) FROM TT WHERE k < 0 OR k >= 300",
    ] {
        assert_eq!(
            rows(&mut s, &q.replace("TT", "p")),
            rows(&mut s, &q.replace("TT", "plain")),
            "{q}"
        );
    }
    // A parameter leaves partitions out when the plan runs.
    let q = "SELECT count(*), sum(id) FROM TT WHERE k = $1";
    for k in [-60i64, 5, 150, 250, 350, 99, 100] {
        let p = [Value::Int(k)];
        assert_eq!(
            with_params(&mut s, &q.replace("TT", "p"), &p),
            with_params(&mut s, &q.replace("TT", "plain"), &p),
            "k = {k}"
        );
    }
    let plan = with_params(
        &mut s,
        "EXPLAIN SELECT * FROM p WHERE k = $1",
        &[Value::Int(5)],
    );
    assert!(
        plan.iter().any(|l| l[0].contains("pruned when run")),
        "{plan:?}"
    );
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
        Vec::<String>::new()
    );
}
