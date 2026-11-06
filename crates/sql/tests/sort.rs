//! `ORDER BY` (PLAN.md §M14.27, sorts): the parallel sort of records —
//! in memory and in scratch space, merged on one thread or in key ranges
//! on several — is a stable sort; and a sort its input's primary-key order
//! already satisfies is not run, with the same rows in the same order, on
//! row and column tables, through a transaction's own writes.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
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

fn sorts(s: &mut Session, q: &str) -> usize {
    rows(s, &format!("EXPLAIN {q}"))
        .iter()
        .filter(|r| r[0].trim_start().starts_with("Sort:"))
        .count()
}

const SETTINGS: [&str; 5] = [
    "SET batch_size = 0",
    "SET batch_size = 1024",
    "SET parallel_workers = 4",
    "SET work_mem = '64kB'",
    "SET parallel_workers = 1",
];

#[test]
fn sorts_are_stable_in_every_setting() {
    // Keys with many ties (and NULLs), of mixed kinds in one column (a
    // UNION's): the order a stable sort of the rows in key order gives.
    let mut s = session(31);
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, g INT, x DOUBLE PRECISION, t TEXT);
         INSERT INTO t SELECT i, (i::bigint * 7919) % 37,
             CASE WHEN i % 11 = 0 THEN NULL ELSE ((i::bigint * 104729) % 1000) / 8.0 END,
             'k' || ((i * 31) % 97)
           FROM generate_series(1, 30000) AS s(i)",
    )
    .unwrap();
    let all = rows(&mut s, "SELECT id, g, x, t FROM t");
    let mut by_key: Vec<&Vec<String>> = all.iter().collect();
    by_key.sort_by_key(|r| r[0].parse::<i64>().unwrap());
    let num = |v: &str| -> Option<f64> { v.parse().ok() };
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        // g ascending, x descending with NULLs last, ties in key order.
        let got = rows(
            &mut s,
            "SELECT id, g, x, t FROM t ORDER BY g, x DESC NULLS LAST",
        );
        let mut want = by_key.clone();
        want.sort_by(|a, b| {
            let g = |r: &Vec<String>| r[1].parse::<i64>().unwrap();
            g(a).cmp(&g(b))
                .then_with(|| match (num(&a[2]), num(&b[2])) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, _) => std::cmp::Ordering::Greater,
                    (_, None) => std::cmp::Ordering::Less,
                    (Some(x), Some(y)) => y.partial_cmp(&x).unwrap(),
                })
        });
        let want: Vec<Vec<String>> = want.into_iter().cloned().collect();
        assert_eq!(got, want, "{setting}");
        // Text keys, descending.
        let got = rows(&mut s, "SELECT id, t FROM t ORDER BY t DESC");
        let mut want: Vec<Vec<String>> = by_key
            .iter()
            .map(|r| vec![r[0].clone(), r[3].clone()])
            .collect();
        want.sort_by(|a, b| b[1].cmp(&a[1]));
        assert_eq!(got, want, "{setting}");
        // A column of integers and doubles (compared by value).
        let got = rows(
            &mut s,
            "SELECT v FROM (SELECT g AS v FROM t WHERE id <= 50
                           UNION ALL SELECT x FROM t WHERE id > 29950) q ORDER BY v",
        );
        let mut want: Vec<f64> = got.iter().filter_map(|r| num(&r[0])).collect();
        let nulls = got.len() - want.len();
        want.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let got_vals: Vec<f64> = got[..want.len()]
            .iter()
            .map(|r| num(&r[0]).unwrap())
            .collect();
        assert_eq!(got_vals, want, "{setting}");
        assert!(
            got[want.len()..].iter().all(|r| r[0] == "NULL"),
            "{setting}: {nulls} NULLs last"
        );
    }
}

#[test]
fn a_sort_the_primary_key_gives_is_not_run() {
    let mut s = session(32);
    for storage in ["row", "column"] {
        s.execute(&format!(
            "DROP TABLE IF EXISTS p; DROP TABLE IF EXISTS c;
             CREATE TABLE p (id BIGINT PRIMARY KEY, x INT) WITH (storage = {storage});
             CREATE TABLE c (a TEXT, b INT, x INT, PRIMARY KEY (a, b)) WITH (storage = {storage});
             INSERT INTO p SELECT (i * 7919) % 20011, i FROM generate_series(1, 20000) AS s(i);
             INSERT INTO c SELECT 'k' || ((i * 31) % 50), (i * 7919) % 20011, i
               FROM generate_series(1, 20000) AS s(i)"
        ))
        .unwrap();
        for (q, want) in [
            ("SELECT x FROM p ORDER BY id", 0),
            ("SELECT id, x FROM p WHERE x % 3 = 0 ORDER BY id", 0),
            ("SELECT id FROM p WHERE id > 100 ORDER BY id, x", 0),
            ("SELECT x FROM p ORDER BY id DESC", 1),
            ("SELECT x FROM p ORDER BY x", 1),
            ("SELECT x, id FROM p ORDER BY id + 0", 1),
            ("SELECT * FROM c ORDER BY a", 0),
            ("SELECT * FROM c ORDER BY a, b, x", 0),
            ("SELECT b, a FROM c ORDER BY a, b", 0),
            ("SELECT * FROM c ORDER BY a, x", 1),
            ("SELECT * FROM c ORDER BY b", 1),
            ("SELECT x FROM c ORDER BY a", 0),
        ] {
            assert_eq!(sorts(&mut s, q), want, "{storage}: {q}");
        }
        let check = |s: &mut Session, what: &str| {
            for setting in SETTINGS {
                s.execute(setting).unwrap();
                let got = rows(s, "SELECT id, x FROM p WHERE x % 3 <> 1 ORDER BY id");
                let ids: Vec<i64> = got.iter().map(|r| r[0].parse().unwrap()).collect();
                assert!(ids.is_sorted(), "{storage} {what} {setting}");
                assert_eq!(
                    got.len(),
                    rows(s, "SELECT count(*) FROM p WHERE x % 3 <> 1")[0][0]
                        .parse::<usize>()
                        .unwrap(),
                    "{storage} {what} {setting}"
                );
                let got = rows(s, "SELECT a, b FROM c ORDER BY a");
                let keys: Vec<(String, i64)> = got
                    .iter()
                    .map(|r| (r[0].clone(), r[1].parse().unwrap()))
                    .collect();
                assert!(keys.is_sorted(), "{storage} {what} {setting}");
            }
        };
        check(&mut s, "committed");
        // A transaction's own writes, among the committed rows.
        s.execute(
            "BEGIN;
             INSERT INTO p SELECT 20011 + i, i FROM generate_series(1, 500) AS s(i);
             INSERT INTO p SELECT -i, i FROM generate_series(1, 500) AS s(i);
             UPDATE p SET x = x + 1 WHERE id % 5 = 0;
             DELETE FROM p WHERE id % 7 = 0;
             INSERT INTO c VALUES ('a', 1, 1), ('k7', -5, 2), ('zz', 0, 3)",
        )
        .unwrap();
        check(&mut s, "in a transaction");
        s.execute("ROLLBACK").unwrap();
    }
}
