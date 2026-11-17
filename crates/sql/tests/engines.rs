//! Tables of the LSM engine (PLAN.md §M14.28 L3): `WITH (engine = lsm)`
//! routes a table's key range — its rows and its indexes — to an LSM
//! tree behind the store, and everything SQL does with the table answers
//! as the B+tree's twin does, before and after a reopen.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn open(p: &Arc<SimPlatform>) -> Arc<Database> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
}

fn session(db: &Arc<Database>) -> Session {
    Session::new(Arc::clone(db) as Arc<dyn Kv>)
}

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
        other => panic!("{other:?}"),
    }
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql).remove(0).remove(0)
}

/// The LSM tree's run files in the database's directory.
fn runs(p: &SimPlatform) -> usize {
    p.fs()
        .list_dir(Path::new("/db/engines"))
        .unwrap_or_default()
        .iter()
        .filter(|n| n.to_string_lossy().starts_with("run-"))
        .count()
}

const QUERIES: &[&str] = &[
    "SELECT count(*), sum(x), min(p), max(x) FROM c_T",
    "SELECT p, count(*) FROM c_T WHERE x > 500 GROUP BY p ORDER BY p LIMIT 5",
    "SELECT id, x FROM c_T WHERE x = 77 ORDER BY id",
    "SELECT * FROM c_T WHERE id BETWEEN 100 AND 110 ORDER BY id",
    "SELECT p.name, count(*) FROM c_T c JOIN p_T p ON p.id = c.p WHERE p.id < 4 GROUP BY p.name ORDER BY 1",
    "SELECT name FROM p_T WHERE name = 'n1!'",
    "SELECT id FROM c_T ORDER BY id DESC LIMIT 3",
];

fn same(s: &mut Session) {
    for q in QUERIES {
        assert_eq!(
            rows(s, &q.replace("_T", "_lsm")),
            rows(s, &q.replace("_T", "_btree")),
            "{q}"
        );
    }
}

#[test]
fn an_lsm_table_answers_as_its_b_tree_twin() {
    let p = Arc::new(SimPlatform::new(7));
    let db = open(&p);
    let mut s = session(&db);
    for engine in ["lsm", "btree"] {
        s.execute(&format!(
            "CREATE TABLE p_{engine} (id INT PRIMARY KEY, name TEXT UNIQUE) WITH (engine = {engine}); \
             CREATE TABLE c_{engine} (id INT PRIMARY KEY, p INT REFERENCES p_{engine} (id), x BIGINT) \
               WITH (engine = {engine}); \
             CREATE INDEX ON c_{engine} (x); \
             INSERT INTO p_{engine} SELECT i, 'n' || i FROM generate_series(1, 300) i; \
             INSERT INTO c_{engine} SELECT i, 1 + i % 300, i * 7 % 1000 FROM generate_series(1, 20000) i; \
             UPDATE c_{engine} SET x = x + 1 WHERE id % 10 = 0; \
             DELETE FROM c_{engine} WHERE id % 7 = 0; \
             UPDATE p_{engine} SET name = name || '!' WHERE id < 5"
        ))
        .unwrap();
    }
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, engine FROM nc_catalog.tables WHERE name LIKE '%_lsm' OR name LIKE '%_btree' ORDER BY name"
        ),
        [
            ["c_btree", "btree"],
            ["c_lsm", "lsm"],
            ["p_btree", "btree"],
            ["p_lsm", "lsm"]
        ]
    );
    same(&mut s);
    // Refused writes leave the LSM table as they leave the B+tree's.
    for engine in ["lsm", "btree"] {
        assert!(
            s.execute(&format!("INSERT INTO p_{engine} VALUES (1, 'again')"))
                .is_err()
        );
        assert!(
            s.execute(&format!("INSERT INTO c_{engine} VALUES (1, 999, 0)"))
                .is_err()
        );
    }
    same(&mut s);
    // A checkpoint saves the LSM tree's runs; a reopen reads them back.
    db.checkpoint().unwrap();
    assert!(runs(&p) > 0, "the LSM tables' keys are in run files");
    drop(s);
    drop(db);
    let db = open(&p);
    let mut s = session(&db);
    same(&mut s);
    // Their versions too: a rolled-back change and a deleted table.
    s.execute("BEGIN; DELETE FROM c_lsm; ROLLBACK").unwrap();
    s.execute("DELETE FROM c_lsm; DELETE FROM c_btree; DELETE FROM p_lsm; DELETE FROM p_btree")
        .unwrap();
    same(&mut s);
    assert_eq!(one(&mut s, "SELECT count(*) FROM c_lsm"), "0");
}

#[test]
fn table_ids_go_to_their_engine() {
    let p = Arc::new(SimPlatform::new(8));
    let db = open(&p);
    let mut s = session(&db);
    let id = |s: &mut Session, t: &str| -> i64 {
        one(
            s,
            &format!("SELECT table_id FROM nc_catalog.tables WHERE name = '{t}'"),
        )
        .parse()
        .unwrap()
    };
    // The id a table would take now.
    s.execute("BEGIN; CREATE TABLE probe (a INT)").unwrap();
    let first = id(&mut s, "probe");
    s.execute("ROLLBACK").unwrap();
    // A rolled-back LSM table leaves its id's range routed (a route is
    // the store's, made at once): a B+tree table passes the id over.
    s.execute("BEGIN; CREATE TABLE gone (a INT) WITH (engine = lsm); ROLLBACK")
        .unwrap();
    s.execute("CREATE TABLE b1 (a INT)").unwrap();
    s.execute("CREATE TABLE l1 (a INT) WITH (engine = lsm)")
        .unwrap();
    assert_eq!(id(&mut s, "b1"), first + 1, "b1 passed over the routed id");
    assert_eq!(id(&mut s, "l1"), first + 2);
    // The setting picks the engine of a table that names none; a
    // partition takes its parent's unless it names one.
    s.execute(
        "SET default_table_engine = lsm; CREATE TABLE l2 (a INT); RESET default_table_engine",
    )
    .unwrap();
    s.execute(
        "CREATE TABLE parts (k INT, v TEXT) PARTITION BY RANGE (k) WITH (engine = lsm); \
         CREATE TABLE parts_1 PARTITION OF parts FOR VALUES FROM (0) TO (100); \
         CREATE TABLE parts_2 PARTITION OF parts FOR VALUES FROM (100) TO (200) WITH (engine = btree); \
         INSERT INTO parts SELECT i, 'v' || i FROM generate_series(0, 199) i; \
         CREATE TABLE copied WITH (engine = lsm) AS SELECT * FROM parts WHERE k % 3 = 0",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, engine FROM nc_catalog.tables WHERE name IN ('l2', 'parts_1', 'parts_2', 'copied') ORDER BY name"
        ),
        [
            ["copied", "lsm"],
            ["l2", "lsm"],
            ["parts_1", "lsm"],
            ["parts_2", "btree"]
        ]
    );
    assert_eq!(one(&mut s, "SELECT count(*) FROM copied"), "67");
    assert!(
        s.execute("CREATE TABLE bad (a INT) WITH (engine = heap)")
            .is_err(),
        "an engine there is not"
    );
    drop(s);
    drop(db);
    let db = open(&p);
    let mut s = session(&db);
    assert_eq!(one(&mut s, "SELECT sum(k) FROM parts"), "19900");
    assert_eq!(one(&mut s, "SELECT count(*) FROM copied"), "67");
}
