//! Erasure (PLAN.md §M14.24 R2): subject columns, `FORGET SUBJECT` over
//! every table that has them — foreign keys followed, all of a subject or
//! nothing under row-level security — and its record, which keeps no value
//! of the subject (nor does the audit log).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

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

fn texts(s: &mut Session, sql: &str) -> Vec<String> {
    rows(s, sql)
        .into_iter()
        .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
        .collect()
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

/// A memory store, messages between subjects (with attachments, which
/// have no subject of their own) and accounts keyed by number.
fn schema(s: &mut Session) {
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 16); \
         CREATE MEMORY STORE notes USING h; \
         SET embedding.pending = wait; \
         CREATE TABLE messages (id int PRIMARY KEY, sender text, recipient text, body text); \
         ALTER TABLE messages SET SUBJECT (sender, recipient); \
         CREATE TABLE attachments (id int PRIMARY KEY, \
             message int REFERENCES messages ON DELETE CASCADE, data text); \
         CREATE TABLE accounts (id bigint PRIMARY KEY, name text); \
         ALTER TABLE accounts SET SUBJECT (id); \
         REMEMBER 'likes green tea' FOR 'alice' IN notes; \
         REMEMBER 'lives in Szeged' FOR 'alice' IN notes; \
         REMEMBER 'plays chess' FOR 'bob' IN notes; \
         INSERT INTO messages VALUES (1, 'alice', 'bob', 'hi bob'), \
             (2, 'bob', 'alice', 'hi alice'), (3, 'bob', 'carol', 'hi carol'); \
         INSERT INTO attachments VALUES (10, 1, 'photo'), (11, 3, 'notes'); \
         INSERT INTO accounts VALUES (7, 'seven'), (8, 'eight')",
    )
    .unwrap();
}

#[test]
fn forget_erases_a_subject_from_every_table() {
    let mut s = Session::new(db());
    schema(&mut s);
    // `accounts` is keyed by number: 'alice' is none of its rows.
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 4"]);
    assert_eq!(
        texts(&mut s, "SELECT subject, content FROM notes"),
        ["bob plays chess"]
    );
    assert_eq!(texts(&mut s, "SELECT id FROM messages"), ["3"]);
    // The cascade followed (not counted: the erasure counts what it deleted).
    assert_eq!(texts(&mut s, "SELECT id FROM attachments"), ["11"]);
    assert_eq!(
        texts(&mut s, "SELECT id FROM accounts ORDER BY id"),
        ["7", "8"]
    );
    // A number is a subject of the text columns and the numeric ones.
    s.execute("INSERT INTO messages VALUES (4, '7', 'bob', 'from seven')")
        .unwrap();
    assert_eq!(texts(&mut s, "FORGET SUBJECT 7"), ["2 2"]);
    assert_eq!(texts(&mut s, "SELECT id FROM accounts"), ["8"]);
    assert_eq!(texts(&mut s, "SELECT id FROM messages"), ["3"]);
    // Nobody: an erasure of nothing, recorded all the same.
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'zed'"), ["3 0"]);
    // The record: who, when, where, how many — and no value of the subject.
    assert_eq!(
        texts(
            &mut s,
            "SELECT erasure, user_name, tables, rows, at <= now() FROM nc_catalog.erasures"
        ),
        [
            "1 nucleus {\"messages: 2\",\"notes: 2\"} 4 t",
            "2 nucleus {\"accounts: 1\",\"messages: 1\"} 2 t",
            "3 nucleus {} 0 t",
        ]
    );
    // Nor does the audit log keep it.
    assert_eq!(
        texts(
            &mut s,
            "SELECT kind, detail FROM nc_catalog.audit WHERE detail LIKE 'FORGET%'"
        ),
        [
            "ddl FORGET SUBJECT '***'",
            "ddl FORGET SUBJECT ***",
            "ddl FORGET SUBJECT '***'"
        ]
    );
    assert_eq!(code(&mut s, "FORGET SUBJECT NULL"), "23502");
    assert_eq!(code(&mut s, "BEGIN; FORGET SUBJECT 'bob'"), "25001");
    s.execute("ROLLBACK").unwrap();
}

#[test]
fn subject_columns_are_checked_and_dropped() {
    let mut s = Session::new(db());
    schema(&mut s);
    assert_eq!(
        code(&mut s, "ALTER TABLE messages SET SUBJECT (nobody)"),
        "42703"
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE messages SET SUBJECT (sender, sender)"),
        "42701"
    );
    s.execute("ALTER TABLE messages SET SUBJECT (recipient)")
        .unwrap();
    // Only as a recipient now.
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'carol'"), ["1 1"]);
    s.execute("ALTER TABLE messages DROP SUBJECT").unwrap();
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'bob'"), ["2 1"]);
    assert_eq!(texts(&mut s, "SELECT count(*) FROM messages"), ["2"]);
}

#[test]
fn foreign_keys_between_tables() {
    let mut s = Session::new(db());
    schema(&mut s);
    // A table without subject columns that restricts: its key decides.
    s.execute(
        "CREATE TABLE replies (id int PRIMARY KEY, \
             message int REFERENCES messages ON DELETE RESTRICT); \
         INSERT INTO replies VALUES (1, 2)",
    )
    .unwrap();
    assert_eq!(code(&mut s, "FORGET SUBJECT 'alice'"), "23503");
    assert_eq!(texts(&mut s, "SELECT count(*) FROM notes"), ["3"]);
    // Marked as the subject's too, it is deleted from first.
    s.execute(
        "ALTER TABLE replies ADD COLUMN author text; \
         UPDATE replies SET author = 'alice'; \
         ALTER TABLE replies SET SUBJECT (author)",
    )
    .unwrap();
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 5"]);
    assert_eq!(texts(&mut s, "SELECT count(*) FROM replies"), ["0"]);
}

#[test]
fn all_of_a_subject_or_nothing() {
    let mut s = Session::new(db());
    schema(&mut s);
    s.execute(
        "CREATE USER app; \
         GRANT SELECT, DELETE ON notes, attachments TO app; \
         GRANT SELECT ON messages, accounts TO app",
    )
    .unwrap();
    s.execute("SET ROLE app").unwrap();
    // No DELETE on `messages`.
    assert_eq!(code(&mut s, "FORGET SUBJECT 'alice'"), "42501");
    s.execute("RESET ROLE").unwrap();
    s.execute(
        "GRANT DELETE ON messages, accounts TO app; \
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY mine ON notes TO app USING (content <> 'lives in Szeged')",
    )
    .unwrap();
    s.execute("SET ROLE app").unwrap();
    // A row of hers it may not see: refused, nothing deleted.
    assert_eq!(code(&mut s, "FORGET SUBJECT 'alice'"), "42501");
    s.execute("RESET ROLE").unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM notes"), ["3"]);
    assert_eq!(texts(&mut s, "SELECT count(*) FROM messages"), ["3"]);
    // Bob's are all visible to it.
    s.execute("SET ROLE app").unwrap();
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'bob'"), ["1 4"]);
    // Its own erasures, not others'.
    assert_eq!(
        texts(&mut s, "SELECT erasure, user_name FROM nc_catalog.erasures"),
        ["1 app"]
    );
    s.execute("RESET ROLE").unwrap();
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM nc_catalog.erasures"),
        ["1"]
    );
}
