//! `COPY … FROM STDIN` in the engine (PLAN.md §M14.11 L5): rows checked
//! as an INSERT's — defaults, types, enums, constraints, foreign keys,
//! privileges, row-level security — and none stored when one fails.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value, parse};

fn session() -> Session {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    Session::new(kv)
}

fn copy(s: &mut Session, sql: &str, data: &str) -> nucleus_sql::Result<Output> {
    let stmt = parse(sql)?.pop().unwrap();
    s.run_copy(&stmt, &[], &mut data.as_bytes())
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

#[test]
fn copy_checks_rows_as_insert_does() {
    let mut s = session();
    s.execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok'); \
         CREATE TABLE parent (id INT PRIMARY KEY); \
         INSERT INTO parent VALUES (1), (2); \
         CREATE TABLE child (id INT PRIMARY KEY, parent INT REFERENCES parent (id), \
           m mood, n NUMERIC(5, 2) CHECK (n >= 0), at TIMESTAMPTZ DEFAULT '2024-01-01 00:00Z', \
           tags TEXT[], code VARCHAR(3))",
    )
    .unwrap();
    let out = copy(
        &mut s,
        "COPY child (id, parent, m, n, tags, code) FROM STDIN",
        "1\t1\tok\t1.234\t{a,b}\tabc\n2\t\\N\tsad\t0\t{}\t\\N\n",
    )
    .unwrap();
    assert_eq!(out, Output::Command("COPY 2".into()));
    assert_eq!(
        rows(
            &mut s,
            "SELECT id, m::text, n::text, at::text, tags[2], code FROM child ORDER BY id"
        ),
        vec![
            vec![
                Value::Int(1),
                Value::Text("ok".into()),
                Value::Text("1.23".into()),
                Value::Text("2024-01-01 00:00:00+00".into()),
                Value::Text("b".into()),
                Value::Text("abc".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("sad".into()),
                Value::Text("0.00".into()),
                Value::Text("2024-01-01 00:00:00+00".into()),
                Value::Null,
                Value::Null,
            ],
        ]
    );
    for (data, code) in [
        ("3\t9\tok\t1\t{}\tx\n", "23503"),    // no parent 9
        ("3\t1\tglad\t1\t{}\tx\n", "22P02"),  // not a label
        ("3\t1\tok\t-1\t{}\tx\n", "23514"),   // the check
        ("3\t1\tok\t1\t{}\tabcd\n", "22001"), // too long
        ("3\t1\tok\tone\t{}\tx\n", "22P02"),  // not a number
        ("3\t1\tok\n", "22P04"),              // too few fields
    ] {
        let e = copy(
            &mut s,
            "COPY child (id, parent, m, n, tags, code) FROM STDIN",
            &format!("10\t1\tok\t1\t{{}}\tx\n{data}"),
        )
        .unwrap_err();
        assert_eq!(e.code, code, "{data:?}: {}", e.message);
    }
    // None of the failed loads' rows stayed.
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM child"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn copy_needs_insert_and_passes_row_security() {
    let mut s = session();
    s.execute(
        "CREATE TABLE notes (id INT PRIMARY KEY, owner TEXT, body TEXT); \
         CREATE USER writer; \
         GRANT INSERT (id, owner) ON notes TO writer; \
         GRANT SELECT ON notes TO writer; \
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY own ON notes USING (owner = current_user) WITH CHECK (owner = current_user)",
    )
    .unwrap();
    s.execute("SET ROLE writer").unwrap();
    // A column without INSERT; a row the policy refuses; then a good one.
    let e = copy(&mut s, "COPY notes FROM STDIN", "1\twriter\thi\n").unwrap_err();
    assert_eq!(e.code, "42501");
    let e = copy(&mut s, "COPY notes (id, owner) FROM STDIN", "1\tsomeone\n").unwrap_err();
    assert_eq!(e.code, "42501");
    copy(
        &mut s,
        "COPY notes (id, owner) FROM STDIN WITH (FORMAT csv)",
        "1,writer\n",
    )
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id FROM notes"),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn copy_without_data_is_refused_and_options_are_checked() {
    let mut s = session();
    s.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
    // `Session::run` has no client to take data from.
    let e = s.execute("COPY t FROM STDIN").unwrap_err();
    assert_eq!(e.code, "0A000");
    for (sql, code) in [
        ("COPY t TO STDOUT", "0A000"),
        ("COPY t FROM '/etc/passwd'", "0A000"),
        ("COPY t FROM STDIN WITH (FORMAT binary)", "0A000"),
        ("COPY t FROM STDIN WITH (DELIMITER 'ab')", "0A000"),
        ("COPY t (a, a) FROM STDIN", "42701"),
        ("COPY t (c) FROM STDIN", "42703"),
    ] {
        let e = copy(&mut s, sql, "").unwrap_err();
        assert_eq!(e.code, code, "{sql}: {}", e.message);
    }
    // The older syntax, a delimiter and a NULL of one's own.
    copy(
        &mut s,
        "COPY t FROM STDIN WITH CSV HEADER DELIMITER ';' NULL 'NA'",
        "a;b\n1;NA\n2;\"x;y\"\n",
    )
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT a, b FROM t ORDER BY a"),
        vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Text("x;y".into())],
        ]
    );
}
