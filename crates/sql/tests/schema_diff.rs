//! The change from one schema to another (PLAN.md §M14.16 M4).
//!
//! The law: `diff(x, y)` run on a database of schema `x` gives schema `y`
//! — for every test kit script's schema from and to an empty database,
//! and for changes a migration makes, both ways.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::schema_diff::{Diff, between};
use nucleus_sql::{Output, Session};
use nucleus_testkit::slt::{Connection, Local, run_script};

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

fn schema(kv: &Arc<dyn Kv>) -> Vec<String> {
    match Session::new(Arc::clone(kv))
        .execute("SELECT definition FROM nc_catalog.definitions ORDER BY position")
        .unwrap()
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows.into_iter().map(|r| r[0].render()).collect(),
        other => panic!("{other:?}"),
    }
}

fn with(sql: &str) -> Arc<dyn Kv> {
    let kv = db();
    let mut s = Session::new(Arc::clone(&kv));
    for stmt in sql.split(";\n").filter(|x| !x.trim().is_empty()) {
        run(&mut s, stmt);
    }
    kv
}

/// Applies `d` to `kv`: an error names the statement.
fn apply(kv: &Arc<dyn Kv>, d: &Diff) -> Result<(), String> {
    let mut s = Session::new(Arc::clone(kv));
    for st in &d.statements {
        s.execute(st)
            .map_err(|e| format!("{st}: {} ({})", e.message, e.code))?;
    }
    Ok(())
}

/// `x` made `y` by their diff, and back by the other.
fn assert_both_ways(x: &str, y: &str) -> (Diff, Diff) {
    let (kx, ky) = (with(x), with(y));
    let up = between(&*kx, &*ky).unwrap();
    let down = between(&*ky, &*kx).unwrap();
    apply(&kx, &up).unwrap_or_else(|e| panic!("up: {e}\n{}", up.script()));
    assert_eq!(schema(&kx), schema(&ky), "up:\n{}", up.script());
    let back = with(y);
    apply(&back, &down).unwrap_or_else(|e| panic!("down: {e}\n{}", down.script()));
    assert_eq!(schema(&back), schema(&with(x)), "down:\n{}", down.script());
    (up, down)
}

#[test]
fn the_same_schema_has_no_diff() {
    let x = "CREATE TABLE t (id int PRIMARY KEY, a text);\nCREATE INDEX t_a ON t (a)";
    assert!(between(&*with(x), &*with(x)).unwrap().is_empty());
}

#[test]
fn a_table_changed_in_place() {
    let (up, down) = assert_both_ways(
        "CREATE TYPE mood AS ENUM ('sad', 'happy');
CREATE TABLE dept (id int PRIMARY KEY, name text);
CREATE TABLE emp (id int PRIMARY KEY, dept_id int, name varchar(20), pay int DEFAULT 0, nick text NOT NULL, old_note text, CONSTRAINT pay_ok CHECK (pay >= 0));
CREATE INDEX emp_name ON emp (name);
COMMENT ON TABLE emp IS 'people';
ALTER TABLE emp ENABLE ROW LEVEL SECURITY;
CREATE POLICY mine ON emp USING (pay < 100)",
        "CREATE TYPE mood AS ENUM ('sad', 'meh', 'happy', 'glad');
CREATE TABLE dept (id int PRIMARY KEY, name text, UNIQUE (name));
CREATE TABLE emp (id int PRIMARY KEY, dept_id int, name varchar(40), pay int DEFAULT 1, nick text, feeling mood, CONSTRAINT pay_ok CHECK (pay > 0));
CREATE INDEX emp_name ON emp (lower(name));
ALTER TABLE emp ADD CONSTRAINT emp_dept FOREIGN KEY (dept_id) REFERENCES dept (id);
COMMENT ON TABLE emp IS 'staff';
CREATE TABLE audit (at timestamp NOT NULL, what text);
ALTER TABLE emp ENABLE ROW LEVEL SECURITY;
ALTER TABLE emp FORCE ROW LEVEL SECURITY;
CREATE POLICY mine ON emp USING (pay < 200)",
    );
    let up = up.script();
    for s in [
        "ALTER TABLE emp DROP COLUMN old_note",
        "ALTER TABLE emp ADD COLUMN feeling mood",
        "ALTER TABLE emp ALTER COLUMN name TYPE varchar(40)",
        "ALTER TABLE emp ALTER COLUMN pay SET DEFAULT 1",
        "ALTER TABLE emp ALTER COLUMN nick DROP NOT NULL",
        "ALTER TYPE mood ADD VALUE 'meh' BEFORE 'happy'",
        "ALTER TYPE mood ADD VALUE 'glad'",
        "CREATE TABLE audit",
    ] {
        assert!(up.contains(s), "{s}\n{up}");
    }
    // The enum cannot lose labels in place: the way back makes it again
    // (it is used: the column goes first).
    assert!(
        down.script().contains("DROP TABLE audit"),
        "{}",
        down.script()
    );
}

#[test]
fn renames_are_noted_not_guessed() {
    let (kx, ky) = (
        with(
            "CREATE TABLE t (id int PRIMARY KEY, a int);\nCREATE TABLE old (id int PRIMARY KEY, x text)",
        ),
        with(
            "CREATE TABLE t (id int PRIMARY KEY, b int);\nCREATE TABLE new (id int PRIMARY KEY, x text)",
        ),
    );
    let d = between(&*kx, &*ky).unwrap();
    let notes = d.notes.join("\n");
    assert!(notes.contains("RENAME COLUMN a TO b"), "{notes}");
    assert!(notes.contains("RENAME TO new"), "{notes}");
    apply(&kx, &d).unwrap();
    assert_eq!(schema(&kx), schema(&ky));
}

#[test]
fn what_a_diff_must_not_do_is_a_note() {
    let (kx, ky) = (
        with("CREATE TABLE t (id int PRIMARY KEY, a int, b int)"),
        with("CREATE TABLE t (a int PRIMARY KEY, id int, b int)"),
    );
    let d = between(&*kx, &*ky).unwrap();
    let notes = d.notes.join("\n");
    assert!(notes.contains("primary key of t"), "{notes}");
    assert!(notes.contains("order"), "{notes}");
}

#[test]
fn a_collections_schema_changed_in_place() {
    let (up, down) = assert_both_ways(
        "CREATE COLLECTION c WITH (schema = '{\"required\": [\"a\"]}');
CREATE COLLECTION gone",
        "CREATE COLLECTION c;
CREATE COLLECTION fresh WITH (schema = '{\"type\": \"object\"}')",
    );
    let (up, down) = (up.script(), down.script());
    for s in [
        "ALTER COLLECTION c RESET (schema)",
        "DROP COLLECTION gone",
        "CREATE COLLECTION fresh",
    ] {
        assert!(up.contains(s), "{s}\n{up}");
    }
    assert!(
        down.contains("ALTER COLLECTION c SET (schema = '{\"required\": [\"a\"]}')"),
        "{down}"
    );
    // Another storage: a note, not a drop (its documents would go).
    let (kx, ky) = (
        with("CREATE COLLECTION c"),
        with("CREATE COLLECTION c WITH (storage = column)"),
    );
    let d = between(&*kx, &*ky).unwrap();
    assert!(d.statements.is_empty(), "{}", d.script());
    assert!(
        d.notes.join("\n").contains("collection c changed"),
        "{:?}",
        d.notes
    );
}

/// Each script's schema, made from nothing and taken back to nothing.
#[test]
fn the_scripts_schemas_from_and_to_nothing() {
    let mut failures = Vec::new();
    for (name, script) in nucleus_testkit::core_scripts() {
        if !nucleus_testkit::picked(name) {
            continue;
        }
        let kv = db();
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Local(Session::new(Arc::clone(&kv)))) };
        run_script(script, name, &mut connect);
        let empty = db();
        let up = between(&*empty, &*kv).unwrap();
        let made = db();
        match apply(&made, &up) {
            Ok(()) if schema(&made) == schema(&kv) => {}
            Ok(()) => failures.push(format!("{name}: up made another schema")),
            Err(e) => failures.push(format!("{name}: up: {e}")),
        }
        let down = between(&*kv, &*empty).unwrap();
        match apply(&made, &down) {
            Ok(()) if schema(&made).is_empty() => {}
            Ok(()) => failures.push(format!("{name}: down left {:?}", schema(&made))),
            Err(e) => failures.push(format!("{name}: down: {e}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
