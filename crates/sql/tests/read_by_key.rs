//! A read by key run as its shape, the key a parameter, one plan for every
//! key (PLAN.md §M14.33 R2, `src/shape.rs`): every answer as the literal
//! read's, and where the rule does not apply, the statement as before.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn session() -> Session {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(7)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    Session::new(kv)
}

/// The rows of `sql`'s one result, each a line of its values.
fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join("|"))
            .collect(),
        Output::Command(c) => panic!("{sql}: {c}"),
    }
}

#[test]
fn every_key_reads_its_own_row() {
    let mut s = session();
    s.execute("CREATE TABLE b (id INT PRIMARY KEY, s TEXT)")
        .unwrap();
    s.execute("INSERT INTO b SELECT i, 'row ' || i FROM generate_series(1, 300) i")
        .unwrap();
    // Twice over: the second round reads through the kept plan.
    for _ in 0..2 {
        for i in 1..=300 {
            assert_eq!(
                rows(&mut s, &format!("SELECT s FROM b WHERE id = {i}")),
                [format!("row {i}")]
            );
        }
    }
    assert!(rows(&mut s, "SELECT s FROM b WHERE id = 301").is_empty());
    // The literal on either side, a qualifier, an alias, more columns.
    assert_eq!(rows(&mut s, "SELECT s FROM b WHERE 7 = id"), ["row 7"]);
    assert_eq!(rows(&mut s, "SELECT s FROM b WHERE b.id = 8"), ["row 8"]);
    assert_eq!(
        rows(&mut s, "SELECT x.s FROM b x WHERE x.id = 9"),
        ["row 9"]
    );
    assert_eq!(
        rows(&mut s, "SELECT id, s FROM b WHERE id = 10 AND s = 'row 10'"),
        ["10|row 10"]
    );
    assert!(rows(&mut s, "SELECT id FROM b WHERE id = 10 AND s = 'row 11'").is_empty());
    // A key past the column's type: no row, as the literal read.
    assert!(rows(&mut s, "SELECT s FROM b WHERE id = 99999999999").is_empty());
    // A change of the table after the plan was kept.
    s.execute("ALTER TABLE b ADD COLUMN t TEXT DEFAULT 'new'")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT s, t FROM b WHERE id = 5"),
        ["row 5|new"]
    );
    assert_eq!(
        rows(&mut s, "SELECT s, t FROM b WHERE id = 6"),
        ["row 6|new"]
    );
    s.execute("UPDATE b SET s = 'changed' WHERE id = 6")
        .unwrap();
    assert_eq!(rows(&mut s, "SELECT s FROM b WHERE id = 6"), ["changed"]);
}

#[test]
fn text_and_composite_keys() {
    let mut s = session();
    s.execute("CREATE TABLE u (name TEXT PRIMARY KEY, n BIGINT)")
        .unwrap();
    s.execute("INSERT INTO u VALUES ('ann', 1), ('o''neil', 2), ('', 3)")
        .unwrap();
    for _ in 0..2 {
        assert_eq!(rows(&mut s, "SELECT n FROM u WHERE name = 'ann'"), ["1"]);
        assert_eq!(
            rows(&mut s, "SELECT n FROM u WHERE name = 'o''neil'"),
            ["2"]
        );
        assert_eq!(rows(&mut s, "SELECT n FROM u WHERE name = ''"), ["3"]);
        assert!(rows(&mut s, "SELECT n FROM u WHERE name = 'bob'").is_empty());
    }
    s.execute("CREATE TABLE c (a SMALLINT, b TEXT, v INT, PRIMARY KEY (a, b))")
        .unwrap();
    s.execute("INSERT INTO c VALUES (1, 'x', 10), (1, 'y', 11), (2, 'x', 20)")
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            rows(&mut s, "SELECT v FROM c WHERE a = 1 AND b = 'y'"),
            ["11"]
        );
        assert_eq!(
            rows(&mut s, "SELECT v FROM c WHERE b = 'x' AND a = 2"),
            ["20"]
        );
        // Part of the key: not a read by key, planned as it comes.
        assert_eq!(
            rows(&mut s, "SELECT v FROM c WHERE a = 1 ORDER BY v"),
            ["10", "11"]
        );
        // Past SMALLINT: no row.
        assert!(rows(&mut s, "SELECT v FROM c WHERE a = 70000 AND b = 'x'").is_empty());
    }
}

#[test]
fn what_is_not_a_read_by_key_runs_as_before() {
    let mut s = session();
    s.execute("CREATE TABLE b (id INT PRIMARY KEY, d DATE, f FLOAT)")
        .unwrap();
    s.execute("INSERT INTO b VALUES (1, '2026-01-02', 1.5), (2, '2026-03-04', 2.5)")
        .unwrap();
    for _ in 0..2 {
        // A string for a date, a float for an integer, a range, an OR.
        assert_eq!(
            rows(&mut s, "SELECT id FROM b WHERE d = '2026-03-04'"),
            ["2"]
        );
        assert_eq!(rows(&mut s, "SELECT id FROM b WHERE id = 2.0"), ["2"]);
        assert_eq!(rows(&mut s, "SELECT id FROM b WHERE id >= 2"), ["2"]);
        assert_eq!(
            rows(
                &mut s,
                "SELECT id FROM b WHERE id = 1 OR id = 2 ORDER BY id"
            ),
            ["1", "2"]
        );
        // A join, a subquery.
        assert_eq!(
            rows(
                &mut s,
                "SELECT x.id FROM b x, b y WHERE x.id = 1 AND y.id = 1"
            ),
            ["1"]
        );
        assert_eq!(
            rows(&mut s, "SELECT id FROM (SELECT id FROM b) t WHERE id = 1"),
            ["1"]
        );
    }
    // Statement with parameters of its own: as before.
    let stmt = nucleus_sql::parse("SELECT f FROM b WHERE id = $1")
        .unwrap()
        .remove(0);
    match s.run(&stmt, &[nucleus_sql::Value::Int(2)]).unwrap() {
        Output::Rows { rows, .. } => assert_eq!(rows[0][0].render(), "2.5"),
        o => panic!("{o:?}"),
    }
    // An unknown column: the error as before.
    let e = s.execute("SELECT id FROM b WHERE nope = 1").unwrap_err();
    assert_eq!(e.code, "42703");
}
