//! Branches in SQL (PLAN.md §M14.13 B3): `CREATE BRANCH`, `DROP
//! BRANCH`, `ALTER BRANCH … RESET`, `nc_catalog.branches` — a branch a
//! database of its own, tables, rows and indexes included, sharing every
//! page it has not changed.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn open(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::clone(p) as Arc<dyn Platform>,
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn texts(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn run(s: &mut Session, sql: &str) {
    s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

#[test]
fn a_branch_is_a_database_of_its_own() {
    let p = Arc::new(SimPlatform::new(1));
    let kv = open(&p);
    let mut main = Session::new(Arc::clone(&kv));
    run(&mut main, "CREATE TABLE t (id int PRIMARY KEY, v text)");
    run(&mut main, "CREATE INDEX t_v ON t (v)");
    run(
        &mut main,
        "INSERT INTO t SELECT i, 'v' || i FROM generate_series(1, 1000) i",
    );
    run(&mut main, "CREATE BRANCH dev");
    let mut dev = Session::new(kv.branch("dev").unwrap());
    assert_eq!(texts(&mut dev, "SELECT count(*) FROM t"), ["1000"]);

    // Writes and schema changes on each side, invisible to the other.
    run(&mut dev, "UPDATE t SET v = 'dev' WHERE id <= 10");
    run(&mut dev, "ALTER TABLE t ADD COLUMN w int");
    run(&mut dev, "CREATE TABLE only_dev (x int)");
    run(&mut main, "DELETE FROM t WHERE id > 500");
    assert_eq!(texts(&mut main, "SELECT count(*) FROM t"), ["500"]);
    assert_eq!(texts(&mut dev, "SELECT count(*) FROM t"), ["1000"]);
    assert_eq!(
        texts(&mut dev, "SELECT count(*) FROM t WHERE v = 'dev'"),
        ["10"]
    );
    assert_eq!(
        texts(&mut main, "SELECT count(*) FROM t WHERE v = 'dev'"),
        ["0"]
    );
    assert_eq!(code(&mut main, "SELECT * FROM only_dev"), "42P01");
    assert_eq!(code(&mut main, "SELECT w FROM t"), "42703");
    assert_eq!(
        texts(&mut dev, "SELECT id FROM t WHERE v = 'v700'"),
        ["700"]
    );

    assert_eq!(
        texts(
            &mut dev,
            "SELECT name, parent, current FROM nc_catalog.branches"
        ),
        ["main NULL f", "dev main t"]
    );
    // A branch of a branch, by name, from the main session.
    run(&mut main, "CREATE BRANCH \"pr-1\" FROM dev");
    let mut pr = Session::new(kv.branch("pr-1").unwrap());
    assert_eq!(
        texts(&mut pr, "SELECT count(*) FROM t WHERE v = 'dev'"),
        ["10"]
    );
    assert_eq!(code(&mut main, "DROP BRANCH dev"), "2BP01");
    run(&mut main, "DROP BRANCH \"pr-1\"");
    assert!(pr.execute("SELECT 1 FROM t").is_err());

    // Reset: its parent as it is now.
    run(&mut main, "ALTER BRANCH dev RESET");
    let mut dev = Session::new(kv.branch("dev").unwrap());
    assert_eq!(texts(&mut dev, "SELECT count(*) FROM t"), ["500"]);
    assert_eq!(code(&mut dev, "SELECT * FROM only_dev"), "42P01");
    run(&mut main, "DROP BRANCH dev");
    run(&mut main, "DROP BRANCH IF EXISTS dev");
    assert_eq!(
        texts(&mut main, "SELECT name FROM nc_catalog.branches"),
        ["main"]
    );
}

#[test]
fn what_cannot_be_done() {
    let p = Arc::new(SimPlatform::new(2));
    let kv = open(&p);
    let mut s = Session::new(Arc::clone(&kv));
    run(&mut s, "CREATE BRANCH a");
    assert_eq!(code(&mut s, "CREATE BRANCH a"), "42P04");
    assert_eq!(code(&mut s, "CREATE BRANCH main"), "42602");
    assert_eq!(code(&mut s, "CREATE BRANCH \"a b\""), "42602");
    assert_eq!(code(&mut s, "CREATE BRANCH b FROM nope"), "3D000");
    assert_eq!(code(&mut s, "DROP BRANCH nope"), "3D000");
    assert_eq!(code(&mut s, "DROP BRANCH main"), "0A000");
    assert_eq!(code(&mut s, "ALTER BRANCH nope RESET"), "3D000");
    let mut a = Session::new(kv.branch("a").unwrap());
    assert_eq!(code(&mut a, "DROP BRANCH a"), "55006");
    run(&mut s, "BEGIN");
    assert_eq!(code(&mut s, "CREATE BRANCH c"), "25001");
    run(&mut s, "ROLLBACK");
    run(&mut s, "CREATE ROLE u LOGIN");
    run(&mut s, "SET ROLE u");
    assert_eq!(code(&mut s, "CREATE BRANCH c"), "42501");
    assert_eq!(code(&mut s, "DROP BRANCH a"), "42501");
}
