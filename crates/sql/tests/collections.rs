//! Collections (PLAN.md §M15 D3) beyond `slt/88_collections.slt`: every
//! storage kept whole through inserts, replacements, upserts, deletes
//! and an index on a field; ids made in the order the documents came;
//! who may make, change and drop one.  (PostgreSQL's clients see a
//! table: pg-compat's `01_pg_catalog.slt`.)

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session};

/// The storages a collection's documents may have.
const STORAGES: &[&str] = &["", "WITH (storage = column)", "WITH (engine = lsm)"];

fn open(seed: u64) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn code(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(_) => "ok".into(),
        Err(e) => e.code.to_string(),
    }
}

/// `sql`'s rows, each rendered.
fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql).remove(0).remove(0)
}

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

#[test]
fn every_storage_keeps_its_documents_and_indexes_whole() {
    for storage in STORAGES {
        let kv = open(1);
        let mut s = Session::new(Arc::clone(&kv));
        s.execute(&format!(
            "CREATE COLLECTION c {storage}; \
             CREATE INDEX c_name ON c ((doc ->> 'name')); \
             INSERT INTO c (doc) SELECT json_build_object('i', i, 'name', 'n' || (i % 7)) \
               FROM generate_series(1, 600) i; \
             INSERT INTO c (doc) SELECT json_build_object('_id', i, 'name', 'given') \
               FROM generate_series(1, 50) i"
        ))
        .unwrap();
        // Made ids follow the order the documents came in.
        assert_eq!(
            one(
                &mut s,
                "SELECT count(*) FROM (SELECT doc ->> 'i' AS i, \
                   row_number() OVER (ORDER BY _id) AS r FROM c WHERE doc ? 'i') x \
                 WHERE i::int <> r"
            ),
            "0",
            "{storage}"
        );
        // Replaced whole (ids kept), by a field, then upserted and deleted.
        s.execute(
            "UPDATE c SET doc = json_build_object('name', 'renamed') WHERE doc ->> 'name' = 'n3'; \
             UPDATE c SET doc = json_set(doc, '{name}', '\"bumped\"') WHERE _id = '7'; \
             INSERT INTO c (doc) SELECT json_build_object('_id', i, 'name', 'up') \
               FROM generate_series(40, 60) i \
               ON CONFLICT (_id) DO UPDATE SET doc = c.doc || json_build_object('seen', true); \
             DELETE FROM c WHERE doc ->> 'name' = 'n5'",
        )
        .unwrap();
        check(&kv, &s, storage);
        assert_eq!(
            rows(
                &mut s,
                "SELECT doc ->> 'name', count(*), count(*) FILTER (WHERE doc ? '_id') \
                 FROM c GROUP BY 1 ORDER BY 1"
            ),
            [
                ["bumped", "1", "1"],
                ["given", "49", "49"],
                ["n0", "85", "85"],
                ["n1", "86", "86"],
                ["n2", "86", "86"],
                ["n4", "86", "86"],
                ["n6", "85", "85"],
                ["renamed", "86", "86"],
                ["up", "10", "10"],
            ]
            .map(|r| r.map(String::from).to_vec())
            .to_vec(),
            "{storage}"
        );
        assert_eq!(
            one(
                &mut s,
                "SELECT count(*) FROM c WHERE doc ->> 'seen' = 'true'"
            ),
            "11",
            "{storage}"
        );
        // The index answers as the scan does.
        assert_eq!(
            one(
                &mut s,
                "SELECT count(*) FROM c WHERE doc ->> 'name' = 'renamed'"
            ),
            "86"
        );
    }
}

#[test]
fn who_may_make_change_and_drop_one() {
    let kv = open(2);
    let mut s = Session::new(kv);
    s.execute("CREATE USER u; CREATE COLLECTION c; GRANT SELECT, INSERT, UPDATE, DELETE ON c TO u")
        .unwrap();
    s.execute("SET ROLE u").unwrap();
    // Writes by the grants; the collection itself is its owner's.
    assert_eq!(
        code(&mut s, "INSERT INTO c (doc) VALUES ('{\"a\": 1}')"),
        "ok"
    );
    assert_eq!(code(&mut s, "UPDATE c SET doc = '{\"a\": 2}'"), "ok");
    assert_eq!(code(&mut s, "ALTER COLLECTION c RENAME TO d"), "42501");
    assert_eq!(
        code(&mut s, "ALTER COLLECTION c SET (schema = '{}')"),
        "42501"
    );
    assert_eq!(code(&mut s, "DROP COLLECTION c"), "42501");
    assert_eq!(code(&mut s, "CREATE COLLECTION mine"), "42501");
    s.execute("RESET ROLE; GRANT CREATE ON SCHEMA public TO u; SET ROLE u")
        .unwrap();
    assert_eq!(code(&mut s, "CREATE COLLECTION mine"), "ok");
    assert_eq!(
        code(
            &mut s,
            "ALTER COLLECTION mine SET (schema = '{\"type\": \"object\"}')"
        ),
        "ok"
    );
    assert_eq!(code(&mut s, "DROP COLLECTION mine"), "ok");
}

#[test]
fn its_definition_keeps_every_option() {
    let mut s = Session::new(open(3));
    s.execute(
        "CREATE COLLECTION a; \
         CREATE COLLECTION b WITH (engine = lsm, schema = '{\"type\": \"object\"}'); \
         CREATE COLLECTION c WITH (storage = column)",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT definition FROM nc_catalog.definitions \
             WHERE kind = 'collection' ORDER BY name"
        ),
        [
            "CREATE COLLECTION a",
            "CREATE COLLECTION b WITH (schema = '{\"type\": \"object\"}', engine = lsm)",
            "CREATE COLLECTION c WITH (storage = column)",
        ]
        .map(|d| vec![d.to_string()])
        .to_vec()
    );
}

#[test]
fn a_path_on_a_table_says_what_a_path_is() {
    let mut s = Session::new(open(4));
    s.execute("CREATE TABLE t (id int PRIMARY KEY, doc json)")
        .unwrap();
    let e = s.execute("CREATE INDEX ON t (doc.a)").unwrap_err();
    assert_eq!(e.code, "42703");
    assert!(
        e.hint
            .as_deref()
            .is_some_and(|h| h.contains("((doc -> 'a'))")),
        "{e:?}"
    );
}
