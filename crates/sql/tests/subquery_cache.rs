//! An execution keeps an uncorrelated subquery's rows (and a correlated
//! one's, by the outer values it reads) by which subquery it is — not by
//! the address of its plan.  A scan copies its filter while it runs, once
//! per partition of a partitioned table; a copy dropped left its address
//! to the next partition's, whose subqueries could lie the other way round:
//! `a IN (SELECT …) AND a NOT IN (SELECT …)` then took one subquery's rows
//! for the other's, and a partition's rows came out wrong (found by a
//! fuzzer partitioning the queries' rows three ways, and comparing a
//! partitioned table with a plain one).

use std::sync::Arc;

use nucleus_kv::{Database, Kv};
use nucleus_sql::{Output, Session, Value};

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    }
}

#[test]
fn a_subquery_keeps_its_own_rows_in_every_partition() {
    let kv: Arc<dyn Kv> =
        Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE plain (id INT PRIMARY KEY, a INT); \
         CREATE TABLE part (id INT PRIMARY KEY, a INT) PARTITION BY HASH (id); \
         CREATE TABLE part_0 PARTITION OF part FOR VALUES WITH (MODULUS 3, REMAINDER 0); \
         CREATE TABLE part_1 PARTITION OF part FOR VALUES WITH (MODULUS 3, REMAINDER 1); \
         CREATE TABLE part_2 PARTITION OF part FOR VALUES WITH (MODULUS 3, REMAINDER 2); \
         INSERT INTO plain SELECT i, CASE WHEN i % 7 = 0 THEN NULL ELSE i % 5 END \
           FROM generate_series(1, 600) i; \
         INSERT INTO part SELECT * FROM plain; \
         CREATE TABLE u (a INT, x INT, y TEXT); \
         INSERT INTO u VALUES (NULL, 9, 'a'), (1, 0, 'b'), (2, 1, 'b'), (3, 5, 'c'), (NULL, 5, 'c')",
    )
    .unwrap();
    // Each subquery's rows differ from the others': confused, the counts
    // change.
    let p = [
        "a IN (SELECT a FROM u WHERE y = 'a') AND a NOT IN (SELECT a FROM u WHERE x < 2)",
        "a NOT IN (SELECT a FROM u WHERE x < 2) AND a IN (SELECT a FROM u WHERE y = 'a')",
        "a IN (SELECT a FROM u WHERE y = 'b') OR a IN (SELECT 4) OR a = (SELECT max(a) FROM u)",
        "EXISTS (SELECT 1 FROM u WHERE u.a = t.a AND u.y = 'b') \
           AND NOT EXISTS (SELECT 1 FROM u WHERE u.a = t.a AND u.x > 0)",
        "a NOT IN (SELECT x FROM u WHERE x IS NOT NULL) \
           AND (a > 3 OR a NOT IN (SELECT a FROM u WHERE x < 2))",
    ];
    for batch in ["0", "1024"] {
        s.execute(&format!("SET batch_size = {batch}")).unwrap();
        for p in p {
            for form in [
                format!("SELECT count(*), sum(id) FROM {{t}} t WHERE {p}"),
                format!("SELECT count(*), sum(id) FROM {{t}} t WHERE NOT ({p})"),
                format!("SELECT count(*), sum(id) FROM {{t}} t WHERE ({p}) IS NULL"),
                format!(
                    "SELECT count(*), sum(id) FROM (SELECT id FROM {{t}} t WHERE ({p}) IS NULL \
                     UNION ALL SELECT id FROM {{t}} t WHERE true \
                     UNION ALL SELECT id FROM {{t}} t WHERE NOT ({p})) s"
                ),
            ] {
                let want = rows(&mut s, &form.replace("{t}", "plain"));
                let got = rows(&mut s, &form.replace("{t}", "part"));
                assert_eq!(got, want, "batch_size {batch}: {form}");
            }
        }
    }
}
