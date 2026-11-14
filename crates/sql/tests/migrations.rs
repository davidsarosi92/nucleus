//! Migrations' records (PLAN.md §M14.16 M2): `RECORD MIGRATION`,
//! `FORGET MIGRATION`, `nc_catalog.migrations` — committed with the
//! change or not at all, per branch, one migrator winning a race.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn run(s: &mut Session, sql: &str) {
    s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

fn recorded(s: &mut Session) -> Vec<String> {
    match s
        .execute("SELECT version, name, checksum, applied_by FROM nc_catalog.migrations")
        .unwrap()
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

#[test]
fn records_commit_with_the_change() {
    let kv = db();
    let mut s = Session::new(Arc::clone(&kv));
    run(
        &mut s,
        "BEGIN; CREATE TABLE t (id int PRIMARY KEY); RECORD MIGRATION 20261114120000 NAME 'init' CHECKSUM 'ab'; COMMIT",
    );
    assert_eq!(recorded(&mut s), ["20261114120000 init ab nucleus"]);
    // Recorded once.
    assert_eq!(
        code(
            &mut s,
            "RECORD MIGRATION 20261114120000 NAME 'init' CHECKSUM 'ab'"
        ),
        "42710"
    );
    // A failed migration: neither its table nor its record.
    run(&mut s, "BEGIN");
    run(&mut s, "CREATE TABLE u (id int)");
    run(&mut s, "RECORD MIGRATION 2 NAME 'u' CHECKSUM 'cd'");
    assert_eq!(code(&mut s, "CREATE TABLE t (id int)"), "42P07");
    run(&mut s, "ROLLBACK");
    assert_eq!(recorded(&mut s).len(), 1);
    assert_eq!(code(&mut s, "SELECT * FROM u"), "42P01");
    // Versions in order, as numbers.
    run(&mut s, "RECORD MIGRATION 3 NAME 'three' CHECKSUM 'x'");
    let v: Vec<_> = recorded(&mut s)
        .into_iter()
        .map(|r| r.split(' ').next().unwrap().to_string())
        .collect();
    assert_eq!(v, ["3", "20261114120000"]);
    // Reverted: the record goes with the change.
    run(
        &mut s,
        "BEGIN; DROP TABLE t; FORGET MIGRATION 20261114120000; COMMIT",
    );
    assert_eq!(recorded(&mut s).len(), 1);
    assert_eq!(code(&mut s, "FORGET MIGRATION 20261114120000"), "42704");
    assert_eq!(
        code(&mut s, "RECORD MIGRATION x NAME 'a' CHECKSUM 'b'"),
        "42601"
    );
}

#[test]
fn a_branch_has_its_own_records() {
    let kv = db();
    let mut s = Session::new(Arc::clone(&kv));
    run(&mut s, "RECORD MIGRATION 1 NAME 'one' CHECKSUM 'a'");
    run(&mut s, "CREATE BRANCH trial");
    let mut b = Session::new(kv.branch("trial").unwrap());
    run(&mut b, "RECORD MIGRATION 2 NAME 'two' CHECKSUM 'b'");
    assert_eq!(recorded(&mut b).len(), 2);
    assert_eq!(recorded(&mut s).len(), 1);
}

#[test]
fn of_two_migrators_one_records() {
    let kv = db();
    let mut a = Session::new(Arc::clone(&kv));
    let mut b = Session::new(Arc::clone(&kv));
    run(&mut a, "BEGIN");
    run(&mut b, "BEGIN");
    run(&mut a, "RECORD MIGRATION 1 NAME 'one' CHECKSUM 'a'");
    run(&mut b, "RECORD MIGRATION 1 NAME 'one' CHECKSUM 'a'");
    run(&mut a, "COMMIT");
    let failed = b.execute("COMMIT");
    assert!(matches!(&failed, Err(e) if e.code == "40001"), "{failed:?}");
    assert_eq!(recorded(&mut a).len(), 1);
}

#[test]
fn recording_needs_the_right_to_create() {
    let kv = db();
    let mut s = Session::new(Arc::clone(&kv));
    run(&mut s, "CREATE ROLE app LOGIN");
    run(&mut s, "SET ROLE app");
    assert_eq!(
        code(&mut s, "RECORD MIGRATION 1 NAME 'one' CHECKSUM 'a'"),
        "42501"
    );
    run(&mut s, "RESET ROLE");
    run(&mut s, "GRANT CREATE ON DATABASE nucleus TO app");
    run(&mut s, "SET ROLE app");
    run(&mut s, "RECORD MIGRATION 1 NAME 'one' CHECKSUM 'a'");
    assert_eq!(recorded(&mut s), ["1 one a app"]);
}
