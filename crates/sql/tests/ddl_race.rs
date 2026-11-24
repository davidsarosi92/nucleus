//! DDL that depends on a table's rows, and a write to them committed while
//! it runs.  The write was made under the old definition — the table's
//! definition it validated was still the old one, the new one committing
//! with the DDL — so the DDL's commit fails (`40001`) rather than keep an
//! index without the write's entries, a constraint the write breaks, or a
//! row of the old layout.  Run again, the DDL sees the write.  Found by
//! transfers between accounts running against `CREATE INDEX` and `ALTER
//! TABLE`: an index on `note` held entries for notes the rows no longer
//! had; then, two sessions at a time, a dropped column whose concurrent
//! insert kept it (its value read as the next column's), a `CHECK` and a
//! `NOT NULL` that rows broke, a column added whose new row had no default
//! (since, a default the same for every row is the rows' without reading
//! them, issue #18 — its case here is a volatile one's).

use std::sync::Arc;

use nucleus_kv::{Database, Kv};
use nucleus_sql::{Output, Session, Value};

/// The DDL, the write committed under it, what the DDL run again gives
/// (`Ok` or an error code), and a query with its one value after.
type Case = (
    &'static str,
    &'static str,
    Result<(), &'static str>,
    Option<(&'static str, Value)>,
);

fn one(s: &mut Session, sql: &str) -> Value {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { mut rows, .. } => rows.remove(0).remove(0),
        o => panic!("{o:?}"),
    }
}

#[test]
fn ddl_fails_over_a_write_committed_under_it() {
    use Value::Int;
    let cases: Vec<Case> = vec![
        (
            "CREATE INDEX t_v ON t (v)",
            "UPDATE t SET v = 'new' WHERE id = 1",
            Ok(()),
            Some(("SELECT count(*) FROM t WHERE v = 'old'", Int(9))),
        ),
        (
            "CREATE INDEX t_e ON t ((k + 1))",
            "UPDATE t SET k = 50 WHERE id = 1",
            Ok(()),
            Some(("SELECT count(*) FROM t WHERE k + 1 = 51", Int(1))),
        ),
        (
            "CREATE UNIQUE INDEX t_u ON t (k)",
            "INSERT INTO t VALUES (100, 'x', 1)",
            Err("23505"),
            None,
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT ck CHECK (v <> 'bad')",
            "UPDATE t SET v = 'bad' WHERE id = 1",
            Err("23514"),
            None,
        ),
        (
            "ALTER TABLE t ALTER COLUMN v SET NOT NULL",
            "UPDATE t SET v = NULL WHERE id = 1",
            Err("23502"),
            None,
        ),
        (
            "ALTER TABLE t ADD FOREIGN KEY (k) REFERENCES p (id)",
            "INSERT INTO t VALUES (100, 'x', 99)",
            Err("23503"),
            None,
        ),
        (
            "ALTER TABLE t ADD FOREIGN KEY (k) REFERENCES p (id)",
            "DELETE FROM p WHERE id = 3",
            Err("23503"),
            None,
        ),
        (
            "ALTER TABLE t ADD COLUMN w float NOT NULL DEFAULT random() + 7",
            "INSERT INTO t VALUES (100, 'x', 1)",
            Ok(()),
            Some(("SELECT w >= 7 FROM t WHERE id = 100", Value::Bool(true))),
        ),
        (
            "ALTER TABLE t ADD COLUMN g int GENERATED ALWAYS AS (k * 2) STORED",
            "INSERT INTO t VALUES (100, 'x', 21)",
            Ok(()),
            Some(("SELECT g FROM t WHERE id = 100", Int(42))),
        ),
        (
            "ALTER TABLE t DROP COLUMN v",
            "INSERT INTO t VALUES (100, 'x', 5)",
            Ok(()),
            Some(("SELECT k FROM t WHERE id = 100", Int(5))),
        ),
        (
            "ALTER TABLE t ALTER COLUMN k TYPE text",
            "INSERT INTO t VALUES (100, 'x', 5)",
            Ok(()),
            Some(("SELECT count(*) FROM t WHERE k = '5'", Int(2))),
        ),
    ];
    for kind in ["", "WITH (storage = column)", "WITH (engine = lsm)"] {
        for (ddl, write, again, after) in &cases {
            let kv: Arc<dyn Kv> =
                Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
            let (mut a, mut b) = (Session::new(kv.clone()), Session::new(kv));
            a.execute(&format!(
                "CREATE TABLE p (id INT PRIMARY KEY); \
                 INSERT INTO p SELECT i FROM generate_series(1, 20) i; \
                 CREATE TABLE t (id INT PRIMARY KEY, v TEXT, k INT) {kind}; \
                 INSERT INTO t SELECT i, 'old', i FROM generate_series(1, 10) i"
            ))
            .unwrap();
            let what = format!("{ddl} [{kind}] under {write}");
            a.execute(&format!("BEGIN ISOLATION LEVEL REPEATABLE READ; {ddl}"))
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
            b.execute(write).unwrap_or_else(|e| panic!("{what}: {e:?}"));
            let e = a.execute("COMMIT").map(|_| ()).unwrap_err();
            assert_eq!(e.code, "40001", "{what}: {e:?}");
            // Run again, it sees the write.
            let got = a.execute(ddl).map(|_| ()).map_err(|e| e.code);
            assert_eq!(got, *again, "{what}, again");
            if let Some((sql, want)) = after {
                assert_eq!(one(&mut b, sql), *want, "{what}: {sql}");
            }
        }
        // A column added with a default the same for every row reads no
        // row (issue #18): the write's row, stored under the old
        // definition, reads the default as the others do.
        let kv: Arc<dyn Kv> =
            Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
        let (mut a, mut b) = (Session::new(kv.clone()), Session::new(kv));
        a.execute(&format!(
            "CREATE TABLE t (id INT PRIMARY KEY, v TEXT, k INT) {kind}; \
             INSERT INTO t SELECT i, 'old', i FROM generate_series(1, 10) i"
        ))
        .unwrap();
        a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ; ALTER TABLE t ADD COLUMN w int NOT NULL DEFAULT 7")
            .unwrap();
        b.execute("INSERT INTO t VALUES (100, 'x', 1)").unwrap();
        a.execute("COMMIT").unwrap();
        assert_eq!(one(&mut b, "SELECT sum(w) FROM t"), Int(77), "[{kind}]");
    }
}
