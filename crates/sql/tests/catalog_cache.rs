//! A session's catalog cache (§M14.20 W8): kept while no commit writes the
//! catalog, never shown to a snapshot that reads the catalog otherwise.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::session::Output;
use nucleus_sql::types::Value;

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn width(s: &mut Session, table: &str) -> usize {
    match s
        .execute(&format!("SELECT * FROM {table}"))
        .unwrap()
        .pop()
        .unwrap()
    {
        Output::Rows { columns, .. } => columns.len(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn another_sessions_ddl_is_seen() {
    let kv = db();
    let (mut a, mut b) = (Session::new(kv.clone()), Session::new(kv));
    a.execute("CREATE TABLE t (x INT); INSERT INTO t VALUES (1)")
        .unwrap();
    assert_eq!(width(&mut b, "t"), 1);
    assert_eq!(width(&mut b, "t"), 1);
    a.execute("ALTER TABLE t ADD COLUMN y INT").unwrap();
    assert_eq!(width(&mut b, "t"), 2);
    a.execute("DROP TABLE t").unwrap();
    assert_eq!(b.execute("SELECT * FROM t").unwrap_err().code, "42P01");
    // Writes outside the catalog leave it be (and are seen).
    a.execute("CREATE TABLE u (x INT)").unwrap();
    assert_eq!(width(&mut b, "u"), 1);
    a.execute("INSERT INTO u VALUES (1), (2)").unwrap();
    assert_eq!(
        rows(&mut b, "SELECT count(*) FROM u"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn a_snapshot_reads_its_own_catalog() {
    let kv = db();
    let (mut a, mut b, mut c) = (
        Session::new(kv.clone()),
        Session::new(kv.clone()),
        Session::new(kv),
    );
    a.execute("CREATE TABLE t (x INT)").unwrap();
    b.execute("BEGIN").unwrap();
    assert_eq!(width(&mut b, "t"), 1);
    a.execute("ALTER TABLE t ADD COLUMN y INT").unwrap();
    // A newer snapshot fills its session's cache with the new definition…
    assert_eq!(width(&mut c, "t"), 2);
    // … the older one keeps reading its own.
    assert_eq!(width(&mut b, "t"), 1);
    b.execute("COMMIT").unwrap();
    assert_eq!(width(&mut b, "t"), 2);
}

#[test]
fn a_transactions_own_ddl_is_seen_and_rolled_back() {
    let kv = db();
    let mut a = Session::new(kv);
    a.execute("CREATE TABLE t (x INT)").unwrap();
    assert_eq!(width(&mut a, "t"), 1);
    a.execute("BEGIN; ALTER TABLE t ADD COLUMN y INT").unwrap();
    assert_eq!(width(&mut a, "t"), 2);
    a.execute("ROLLBACK").unwrap();
    assert_eq!(width(&mut a, "t"), 1);
    // Serializable, writing: the catalog read as it is.
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE; INSERT INTO t VALUES (1)")
        .unwrap();
    assert_eq!(width(&mut a, "t"), 1);
    a.execute("COMMIT").unwrap();
}

#[test]
fn privileges_and_settings_are_seen() {
    let kv = db();
    let mut admin = Session::new(kv.clone());
    admin
        .execute("CREATE TABLE t (x INT); CREATE USER reader")
        .unwrap();
    let mut r = Session::new(kv.clone());
    r.login("reader").unwrap();
    assert_eq!(r.execute("SELECT * FROM t").unwrap_err().code, "42501");
    admin.execute("GRANT SELECT ON t TO reader").unwrap();
    assert_eq!(width(&mut r, "t"), 1);
    admin.execute("REVOKE SELECT ON t FROM reader").unwrap();
    assert_eq!(r.execute("SELECT * FROM t").unwrap_err().code, "42501");
    // Through a role.
    admin
        .execute("CREATE ROLE readers; GRANT SELECT ON t TO readers; GRANT readers TO reader")
        .unwrap();
    assert_eq!(width(&mut r, "t"), 1);
    admin.execute("REVOKE readers FROM reader").unwrap();
    assert_eq!(r.execute("SELECT * FROM t").unwrap_err().code, "42501");
    // A database setting.
    let show = |s: &mut Session| {
        rows(
            s,
            "SELECT value FROM nc_catalog.database_settings WHERE name = 'password_min_length'",
        )
    };
    assert_eq!(show(&mut admin), vec![vec![Value::Text("1".into())]]);
    admin
        .execute("ALTER DATABASE nucleus SET password_min_length = 8")
        .unwrap();
    let mut other = Session::new(kv);
    assert_eq!(
        other
            .execute("CREATE USER short PASSWORD 'short'")
            .unwrap_err()
            .code,
        "22023"
    );
}

#[test]
fn a_kept_plan_is_checked_for_who_runs_it() {
    let kv = db();
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE t (x INT, owner TEXT); INSERT INTO t VALUES (1, 'a'), (2, 'b'); \
         CREATE USER a; CREATE USER b; GRANT SELECT ON t TO a; GRANT SELECT ON t TO b; \
         ALTER TABLE t ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY mine ON t USING (owner = current_user)",
    )
    .unwrap();
    let q = "SELECT x FROM t ORDER BY x";
    assert_eq!(
        rows(&mut s, q),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    assert_eq!(rows(&mut s, q).len(), 2);
    // (A plan is kept once its text comes again: each twice.)
    s.execute("SET ROLE a").unwrap();
    for _ in 0..2 {
        assert_eq!(rows(&mut s, q), vec![vec![Value::Int(1)]]);
    }
    s.execute("SET ROLE b").unwrap();
    for _ in 0..2 {
        assert_eq!(rows(&mut s, q), vec![vec![Value::Int(2)]]);
    }
    s.execute("RESET ROLE; CREATE USER c; SET ROLE c").unwrap();
    assert_eq!(s.execute(q).unwrap_err().code, "42501");
}

#[test]
fn a_kept_plan_still_notes_its_tables_for_the_audit_log() {
    let kv = db();
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE pay (x INT); CREATE TABLE other (x INT); \
         ALTER DATABASE nucleus SET audit_tables = 'pay'",
    )
    .unwrap();
    for _ in 0..3 {
        s.execute("SELECT * FROM pay").unwrap();
        s.execute("SELECT * FROM other").unwrap();
    }
    assert_eq!(
        rows(
            &mut s,
            "SELECT count(*) FROM nc_catalog.audit WHERE detail = 'SELECT * FROM pay'"
        ),
        vec![vec![Value::Int(3)]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT count(*) FROM nc_catalog.audit WHERE detail = 'SELECT * FROM other'"
        ),
        vec![vec![Value::Int(0)]]
    );
}

#[test]
fn an_entry_by_text_serves_the_same_statement_only() {
    let kv = db();
    let mut s = Session::new(kv);
    s.execute("CREATE TABLE t (x INT, y TEXT)").unwrap();
    let parse = |sql: &str| nucleus_sql::parser::parse(sql).unwrap().pop().unwrap();
    let (a, b) = (
        parse("SELECT x FROM t WHERE x = $1"),
        parse("SELECT y, x FROM t WHERE y = $1"),
    );
    // A text noted that is not the statement's (a front end's mistake);
    // the first kept once seen again.
    let mut describe = |stmt| {
        s.note_text("the same text");
        s.describe(stmt).unwrap()
    };
    let (pa, ca) = describe(&a);
    assert_eq!(describe(&a), (pa.clone(), ca.clone()));
    s.note_text("the same text");
    let (pb, cb) = s.describe(&b).unwrap();
    assert_ne!(pa, pb);
    assert_eq!(ca.unwrap().len(), 1);
    assert_eq!(cb.unwrap().len(), 2);
    // A plan likewise.
    s.execute("INSERT INTO t VALUES (1, 'one')").unwrap();
    let (a, b) = (parse("SELECT x FROM t"), parse("SELECT y FROM t"));
    let mut run = |stmt| {
        s.note_text("the same text");
        match s.run(stmt, &[]).unwrap() {
            Output::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        }
    };
    for _ in 0..2 {
        assert_eq!(run(&a), vec![vec![Value::Int(1)]]);
    }
    assert_eq!(run(&b), vec![vec![Value::Text("one".into())]]);
}
