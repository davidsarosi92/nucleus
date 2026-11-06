//! Adaptive joins (PLAN.md §M14.12, after V3): a hash join whose right
//! side turns out smaller than where looking the left side up costs the
//! same gives its rows by lookups — the same rows as the hash join, in
//! every executor, on row and column tables; a right side as large as
//! estimated stays a hash join.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn session(db: &Database, setting: &str) -> Session {
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(setting).unwrap();
    s
}

/// A statement's rows (rendered) or its error.
fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            other => panic!("{sql}: {other:?}"),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

fn sorted(r: Result<Vec<Vec<String>>, String>) -> Result<Vec<Vec<String>>, String> {
    r.map(|mut rows| {
        rows.sort();
        rows
    })
}

fn plan(s: &mut Session, sql: &str) -> String {
    outcome(s, &format!("EXPLAIN {sql}"))
        .unwrap()
        .iter()
        .map(|r| r.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn adaptive_joins_give_the_hash_joins_rows() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(17)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, "SET batch_size = 1024");
    // `g`'s keys 0..3000 three times each, and 7 and 11 forty times more:
    // a HAVING over its groups is estimated at a third of them.
    s.execute(
        "CREATE TABLE f (id INT PRIMARY KEY, k INT, x INT);
         INSERT INTO f SELECT i, i % 3000, i % 97 FROM generate_series(1, 30000) AS t(i);
         CREATE INDEX f_k ON f (k);
         CREATE TABLE fc (id INT PRIMARY KEY, k INT, x INT) WITH (storage = column);
         INSERT INTO fc SELECT * FROM f;
         CREATE INDEX fc_k ON fc (k);
         CREATE TABLE g (id INT PRIMARY KEY, k INT);
         INSERT INTO g SELECT i, i % 3000 FROM generate_series(1, 9000) AS t(i);
         INSERT INTO g SELECT 9000 + i, CASE WHEN i % 2 = 0 THEN 7 ELSE 11 END
             FROM generate_series(1, 80) AS t(i);
         ANALYZE",
    )
    .unwrap();
    let settings = [
        "SET batch_size = 0",
        "SET batch_size = 3",
        "SET batch_size = 1024",
        "SET parallel_workers = 4",
        "SET parallel_workers = 4; SET work_mem = '64kB'",
    ];
    let mut sessions: Vec<Session> = settings.iter().map(|q| session(&db, q)).collect();
    for table in ["f", "fc"] {
        // (a, the HAVING) few right rows: lookups; (b) as many as
        // estimated: the hash join; (c) a condition beyond the keys.
        let cases = [
            ("count(*) > 10", "", true),
            ("count(*) > 2", "", false),
            ("count(*) > 10", " AND l.x % 2 = 0", true),
        ];
        for (having, more, few) in cases {
            let q = |key: &str| {
                format!(
                    "SELECT l.id, l.x, r.k, r.n FROM {table} l \
                     JOIN (SELECT k, count(*) AS n FROM g GROUP BY k HAVING {having}) r \
                     ON {key} = r.k{more}"
                )
            };
            let adaptive = q("l.k");
            // Without an index on the key: the hash join alone.
            let reference = sorted(outcome(&mut s, &q("l.k + 0")));
            assert!(
                plan(&mut s, &adaptive).contains("or lookups up to"),
                "{table} {having}: {}",
                plan(&mut s, &adaptive)
            );
            assert!(!plan(&mut s, &q("l.k + 0")).contains("lookups"));
            let rows = reference.as_ref().unwrap().len();
            assert!(rows > 0, "{table} {having}{more}");
            let mut first = None;
            for (setting, session) in settings.iter().zip(&mut sessions) {
                let before = nucleus_sql::vector::adaptive_lookups();
                let got = outcome(session, &adaptive);
                let looked_up = nucleus_sql::vector::adaptive_lookups() > before;
                assert_eq!(looked_up, few, "{table} {having}{more}: {setting}");
                assert_eq!(
                    sorted(got.clone()),
                    reference,
                    "{table} {having}{more}: {setting}"
                );
                // The same order in every executor (within work_mem).
                if !setting.contains("work_mem") {
                    match &first {
                        None => first = Some(got),
                        Some(f) => assert_eq!(&got, f, "{table} {having}{more}: {setting}"),
                    }
                }
            }
        }
    }
}
