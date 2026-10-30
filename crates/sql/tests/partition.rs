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

#[test]
fn a_storage_policy_converts_partitions_as_they_age() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(5)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE ev (at TIMESTAMP, n INT, PRIMARY KEY (at, n)) PARTITION BY RANGE (at); \
         CREATE TABLE ev_1 PARTITION OF ev FOR VALUES FROM ('2030-01-01') TO ('2030-02-01'); \
         CREATE TABLE ev_2 PARTITION OF ev FOR VALUES FROM ('2030-02-01') TO ('2030-03-01'); \
         CREATE TABLE ev_3 PARTITION OF ev FOR VALUES FROM ('2030-03-01') TO ('2030-04-01'); \
         INSERT INTO ev SELECT timestamp '2030-01-01' + i * interval '1 hour', i \
         FROM generate_series(0, 2000) i; \
         ALTER TABLE ev SET STORAGE POLICY (row, column AFTER interval '7 days')",
    )
    .unwrap();
    let storages = |s: &mut Session| {
        rows(
            s,
            "SELECT name, storage FROM nc_catalog.tables WHERE name LIKE 'ev_%' ORDER BY name",
        )
        .into_iter()
        .map(|r| r[1].clone())
        .collect::<Vec<_>>()
    };
    // Every range still ahead of now (the simulated clock is long before).
    assert_eq!(storages(&mut s), ["row", "row", "row"]);
    let before = rows(&mut s, "SELECT * FROM ev ORDER BY at");
    let at = |day: &str| {
        let d = nucleus_sql::datetime::parse_timestamp(day, false).unwrap();
        nucleus_sql::partition::apply_policies(&kv, None, d).unwrap();
    };
    // Six days after January: not yet.
    at("2030-02-07 00:00:00");
    assert_eq!(storages(&mut s), ["row", "row", "row"]);
    at("2030-02-08 00:00:00");
    assert_eq!(storages(&mut s), ["row,column", "row", "row"]);
    // Again: nothing more to do.
    at("2030-02-09 00:00:00");
    assert_eq!(storages(&mut s), ["row,column", "row", "row"]);
    at("2031-01-01 00:00:00");
    assert_eq!(storages(&mut s), ["row,column", "row,column", "row,column"]);
    assert_eq!(rows(&mut s, "SELECT * FROM ev ORDER BY at"), before);
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    assert!(
        nucleus_sql::check::check(&*txn, s.registry())
            .unwrap()
            .is_empty()
    );
    drop(txn);
    // No policy: nothing converts.
    s.execute(
        "ALTER TABLE ev SET STORAGE POLICY NONE; \
         CREATE TABLE ev_4 PARTITION OF ev FOR VALUES FROM ('2030-04-01') TO ('2030-05-01')",
    )
    .unwrap();
    at("2032-01-01 00:00:00");
    assert_eq!(storages(&mut s)[3], "row");
}
