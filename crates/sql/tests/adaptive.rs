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
                let before = nucleus_sql::vector::adaptive_lookups_here();
                let got = outcome(session, &adaptive);
                let looked_up = nucleus_sql::vector::adaptive_lookups_here() > before;
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

/// A recursive query's step joining its last round to a table (PLAN.md
/// §M14.34 G1): an adaptive hash join, not index lookups fixed by the
/// round's constant estimate — each round looks up where its rows are
/// few and hashes them where they are many, with the rows of the hash
/// join alone (no index), in every executor.
#[test]
fn recursive_rounds_choose_lookups_or_the_hash_join() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(23)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, "SET batch_size = 1024");
    s.execute(
        "CREATE TABLE e (id INT PRIMARY KEY, parent INT, v INT);
         INSERT INTO e SELECT i, i / 2, i % 7 FROM generate_series(1, 30000) AS t(i);
         CREATE INDEX e_parent ON e (parent);
         ANALYZE",
    )
    .unwrap();
    let settings = [
        "SET batch_size = 0",
        "SET batch_size = 1024",
        "SET parallel_workers = 4",
    ];
    let mut sessions: Vec<Session> = settings.iter().map(|q| session(&db, q)).collect();
    // (walk, every round few): a node's descendants, the whole tree's,
    // with UNION and a depth, a node's ancestors (by the primary key).
    let walks = [
        (
            "WITH RECURSIVE d (id) AS (SELECT 1000 UNION ALL \
             SELECT e.id FROM e JOIN d ON {key} = d.id) SELECT id FROM d",
            true,
        ),
        (
            "WITH RECURSIVE d (id) AS (SELECT 1 UNION ALL \
             SELECT e.id FROM e JOIN d ON {key} = d.id) SELECT id FROM d",
            false,
        ),
        (
            "WITH RECURSIVE d (id, depth) AS (SELECT 1, 0 UNION \
             SELECT e.id, d.depth + 1 FROM e JOIN d ON {key} = d.id AND e.v <> 3) \
             SELECT id, depth FROM d",
            false,
        ),
    ];
    for (walk, few) in walks {
        let adaptive = walk.replace("{key}", "e.parent");
        let reference = sorted(outcome(&mut s, &walk.replace("{key}", "e.parent + 0")));
        assert!(reference.as_ref().unwrap().len() > 10, "{walk}");
        let p = plan(&mut s, &adaptive);
        assert!(p.contains("or lookups up to"), "{walk}: {p}");
        // Which rounds hashed: the table's scan ran for none of them, or
        // for the large ones — in the pipelines and row at a time.
        for session in [&mut s, &mut sessions[0]] {
            let analyzed = plan(session, &format!("ANALYZE {adaptive}"));
            let scan = analyzed
                .lines()
                .find(|l| l.contains("Seq Scan on e"))
                .unwrap_or_else(|| panic!("{walk}: {analyzed}"));
            assert_eq!(scan.contains("never executed"), few, "{walk}: {analyzed}");
        }
        // The same rounds looked up in every executor.
        let mut rounds = None;
        for (setting, session) in settings.iter().zip(&mut sessions) {
            let before = nucleus_sql::vector::adaptive_lookups_here();
            assert_eq!(
                sorted(outcome(session, &adaptive)),
                reference,
                "{walk}: {setting}"
            );
            let looked_up = nucleus_sql::vector::adaptive_lookups_here() - before;
            assert!(looked_up > 0, "{walk}: {setting}");
            match rounds {
                None => rounds = Some(looked_up),
                Some(r) => assert_eq!(looked_up, r, "{walk}: {setting}"),
            }
        }
    }
    let ancestors = "WITH RECURSIVE a (id, p) AS (SELECT id, parent FROM e WHERE id = 29999 \
                     UNION ALL SELECT e.id, e.parent FROM e JOIN a ON e.id = a.p) SELECT id FROM a";
    let p = plan(&mut s, ancestors);
    assert!(p.contains("or lookups up to"), "{p}");
    for session in &mut sessions {
        assert_eq!(outcome(session, ancestors).unwrap().len(), 15);
    }
}

#[test]
fn joins_over_a_recursive_query_choose_by_its_rows() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(29)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, "SET batch_size = 1024");
    s.execute(
        "CREATE TABLE e (id INT PRIMARY KEY, parent INT, v INT);
         INSERT INTO e SELECT i, i / 2, i % 7 FROM generate_series(1, 30000) AS t(i);
         CREATE INDEX e_parent ON e (parent);
         ANALYZE",
    )
    .unwrap();
    let settings = [
        "SET batch_size = 0",
        "SET batch_size = 1024",
        "SET parallel_workers = 4",
    ];
    let mut sessions: Vec<Session> = settings.iter().map(|q| session(&db, q)).collect();
    let walk = "WITH RECURSIVE d (id) AS (SELECT {root} UNION ALL \
                SELECT e.id FROM e JOIN d ON e.parent = d.id) ";
    // (root, whether its rows are few: 31 of them, or the whole tree's)
    for (root, few) in [("1000", true), ("1", false)] {
        let walk = walk.replace("{root}", root);
        // The walk's own rounds' lookups, then each join's past them.
        let mut alone = Vec::new();
        for session in &mut sessions {
            let before = nucleus_sql::vector::adaptive_lookups_here();
            outcome(session, &format!("{walk}SELECT id FROM d")).unwrap();
            alone.push(nucleus_sql::vector::adaptive_lookups_here() - before);
        }
        for join in [
            "SELECT d.id, e.v FROM d JOIN e ON e.id{plus} = d.id",
            "SELECT id, v FROM e WHERE id{plus} IN (SELECT id / 2 FROM d)",
        ] {
            let query = format!("{walk}{}", join.replace("{plus}", ""));
            let reference = sorted(outcome(
                &mut s,
                &format!("{walk}{}", join.replace("{plus}", " + 0")),
            ));
            assert!(reference.as_ref().unwrap().len() > 10, "{query}");
            let p = plan(&mut s, &query);
            assert_eq!(p.matches("or lookups up to").count(), 2, "{query}: {p}");
            for ((setting, session), alone) in settings.iter().zip(&mut sessions).zip(&alone) {
                let before = nucleus_sql::vector::adaptive_lookups_here();
                assert_eq!(
                    sorted(outcome(session, &query)),
                    reference,
                    "{query}: {setting}"
                );
                let looked_up = nucleus_sql::vector::adaptive_lookups_here() - before;
                assert_eq!(looked_up, alone + u64::from(few), "{query}: {setting}");
            }
        }
    }
}
