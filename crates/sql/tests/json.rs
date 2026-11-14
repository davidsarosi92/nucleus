//! `json` (PLAN.md §M14.15 J1) through SQL: stored, read back after a
//! reopen, compared, ordered, grouped, a key and an index, cast — and its
//! numbers exact at any size.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn open(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(p.clone(), Path::new("/db"), nucleus_kv::Options::default()).unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" | "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn error(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(out) => panic!("{sql}: {out:?}"),
        Err(e) => format!("{} {}", e.code, e.message),
    }
}

#[test]
fn stored_and_read_back() {
    let p = Arc::new(SimPlatform::new(7));
    {
        let mut s = Session::new(open(&p));
        s.execute("CREATE TABLE docs (id INT PRIMARY KEY, doc JSONB, note TEXT)")
            .unwrap();
        s.execute(
            "INSERT INTO docs VALUES
               (1, '{\"name\": \"Ann\", \"tags\": [\"a\", \"b\"], \"age\": 41}', 'x'),
               (2, '[1, 2.50, 1e2, 123456789012345678901234567890]', 'y'),
               (3, '\"just a string\"', 'z'),
               (4, NULL, 'w'),
               (5, '{\"b\": 1, \"a\": {\"z\": null}, \"b\": 2}', 'v')",
        )
        .unwrap();
        // A large one: out of line, past the row's threshold.
        let big: String = (0..5000)
            .map(|i| format!("\"k{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        s.execute(&format!("INSERT INTO docs VALUES (6, '{{{big}}}', 'u')"))
            .unwrap();
    }
    // Reopened: read from the disk.
    let mut s = Session::new(open(&p));
    assert_eq!(
        rows(&mut s, "SELECT id, doc FROM docs WHERE id < 6 ORDER BY id"),
        [
            "1 | {\"age\": 41, \"name\": \"Ann\", \"tags\": [\"a\", \"b\"]}",
            "2 | [1, 2.5, 100, 123456789012345678901234567890]",
            "3 | \"just a string\"",
            "4 | NULL",
            "5 | {\"a\": {\"z\": null}, \"b\": 2}",
        ]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT length(doc::text) > 50000, doc = doc FROM docs WHERE id = 6"
        ),
        ["t | t"]
    );
    // An update of another column keeps it.
    s.execute("UPDATE docs SET note = 'changed' WHERE id = 1")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT doc FROM docs WHERE id = 1"),
        ["{\"age\": 41, \"name\": \"Ann\", \"tags\": [\"a\", \"b\"]}"]
    );
}

#[test]
fn compared_ordered_grouped_and_keyed() {
    let p = Arc::new(SimPlatform::new(8));
    let mut s = Session::new(open(&p));
    // Equal documents, however written, are one value.
    assert_eq!(
        rows(
            &mut s,
            "SELECT '{\"a\": 1, \"b\": [1.0, 2]}'::json = '{\"b\":[1,2.00],\"a\":1}'::json,
                    '1'::json = '1.0'::json, '\"1\"'::json = '1'::json"
        ),
        ["t | t | f"]
    );
    s.execute("CREATE TABLE k (j JSON PRIMARY KEY, n INT)")
        .unwrap();
    s.execute(
        "INSERT INTO k VALUES ('{\"a\": 1}', 1), ('[1]', 2), ('\"s\"', 3), ('2', 4), ('null', 5),
                              ('true', 6), ('[1, 2]', 7), ('{}', 8), ('-1.5', 9), ('\"\"', 10)",
    )
    .unwrap();
    // PostgreSQL's order: null < string < number < boolean < array < object.
    assert_eq!(
        rows(&mut s, "SELECT j FROM k ORDER BY j"),
        [
            "null",
            "\"\"",
            "\"s\"",
            "-1.5",
            "2",
            "true",
            "[1]",
            "[1, 2]",
            "{}",
            "{\"a\": 1}"
        ]
    );
    // The key finds a document written another way.
    assert_eq!(
        rows(&mut s, "SELECT n FROM k WHERE j = '{ \"a\" : 1.0 }'"),
        ["1"]
    );
    assert!(error(&mut s, "INSERT INTO k VALUES ('{\"a\":1}', 11)").starts_with("23505"));
    // A range over the key: numbers between 0 and 10.
    assert_eq!(
        rows(
            &mut s,
            "SELECT j FROM k WHERE j > '0' AND j < '10' ORDER BY j"
        ),
        ["2"]
    );
    // DISTINCT and GROUP BY on equal documents.
    assert_eq!(
        rows(
            &mut s,
            "SELECT j, count(*) FROM (VALUES ('{\"x\": [1, 2]}'::json), ('{\"x\":[1.0,2]}'),
               ('[]')) v(j) GROUP BY j ORDER BY j"
        ),
        ["[] | 1", "{\"x\": [1, 2]} | 2"]
    );
    // A secondary index, and min / max.
    s.execute("CREATE TABLE e (id INT PRIMARY KEY, j JSON)")
        .unwrap();
    s.execute("CREATE INDEX e_j ON e (j)").unwrap();
    s.execute(
        "INSERT INTO e SELECT i, ('{\"i\": ' || i || '}')::json FROM generate_series(1, 200) i",
    )
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id FROM e WHERE j = '{\"i\": 150}'"),
        ["150"]
    );
    assert_eq!(
        rows(&mut s, "SELECT min(j), max(j) FROM e"),
        ["{\"i\": 1} | {\"i\": 200}"]
    );
}

#[test]
fn casts_and_errors() {
    let p = Arc::new(SimPlatform::new(9));
    let mut s = Session::new(open(&p));
    assert_eq!(
        rows(
            &mut s,
            "SELECT '42'::json::int, '1.25'::jsonb::numeric, 'true'::json::boolean,
                    '1e2'::json::float8, '{\"a\": 1}'::json::text"
        ),
        ["42 | 1.25 | t | 100 | {\"a\": 1}"]
    );
    assert_eq!(
        error(&mut s, "SELECT '\"x\"'::json::int"),
        "22023 cannot cast json string to type integer"
    );
    assert!(
        error(&mut s, "SELECT '{\"a\": }'::json")
            .starts_with("22P02 invalid input syntax for type json")
    );
    assert!(error(&mut s, "SELECT '\"\\u0000\"'::json").starts_with("22P05"));
    assert!(error(&mut s, "SELECT '1e999999'::json").starts_with("22003"));
    assert!(error(&mut s, "SELECT '1e300'::json::numeric").starts_with("22003"));
    // Exact at any size, within numeric's bounds.
    assert_eq!(
        rows(
            &mut s,
            "SELECT '0.000000000000000000000000000000000000000001'::json"
        ),
        ["0.000000000000000000000000000000000000000001"]
    );
}
