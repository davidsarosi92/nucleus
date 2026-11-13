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
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 4 t 0"]);
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
    assert_eq!(texts(&mut s, "FORGET SUBJECT 7"), ["2 2 t 0"]);
    assert_eq!(texts(&mut s, "SELECT id FROM accounts"), ["8"]);
    assert_eq!(texts(&mut s, "SELECT id FROM messages"), ["3"]);
    // Nobody: an erasure of nothing, recorded all the same.
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'zed'"), ["3 0 t 0"]);
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
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'carol'"), ["1 1 t 0"]);
    s.execute("ALTER TABLE messages DROP SUBJECT").unwrap();
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'bob'"), ["2 1 t 0"]);
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
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 5 t 0"]);
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
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'bob'"), ["1 4 t 0"]);
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

/// What a value is searched for as: its bytes.
fn needle(s: &str) -> Vec<Vec<u8>> {
    vec![s.as_bytes().to_vec()]
}

#[test]
fn the_scrub_leaves_none_of_the_values_in_any_file() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 16); \
         CREATE MEMORY STORE notes USING h; \
         SET embedding.pending = wait; \
         CREATE TABLE messages (id int PRIMARY KEY, sender text, body text); \
         ALTER TABLE messages SET SUBJECT (sender); \
         CREATE INDEX ON messages USING text (body); \
         CREATE TABLE ledger (id int PRIMARY KEY, owner text, line text) \
             WITH (storage = column); \
         ALTER TABLE ledger SET SUBJECT (owner); \
         INSERT INTO ledger SELECT g, CASE WHEN g % 100 = 0 THEN 'alice' ELSE 'bob' END, \
             CASE WHEN g % 100 = 0 THEN 'alice' ELSE 'bob' END || ' paid kestrel-' || g \
             FROM generate_series(1, 5000) g",
    )
    .unwrap();
    for i in 0..60 {
        let who = if i % 3 == 0 { "alice" } else { "bob" };
        s.execute(&format!(
            "REMEMBER '{who} remembers quokka-{i:03} at the harbour' FOR '{who}' IN notes; \
             INSERT INTO messages VALUES ({i}, '{who}', '{who} wrote zephyr-{i:03} to everyone')"
        ))
        .unwrap();
    }
    // The vectors and terms folded into graphs and segments first.
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    let hers = "alice remembers quokka-003 at the harbour";
    let his = "bob remembers quokka-004 at the harbour";
    assert!(kv.find_bytes(&needle(hers)).unwrap().unwrap() > 0);
    // Similar memories merged (9 of her 20 kept): what a merge replaced
    // is an old version only, and goes all the same.
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM notes WHERE subject = 'alice'"),
        ["9"]
    );
    // Her ledger lines are in column segments (one statement wrote enough).
    assert!(
        kv.find_bytes(&needle("alice paid kestrel-2500"))
            .unwrap()
            .unwrap()
            > 0
    );
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 79 t 0"]);
    // None of hers anywhere — the rows, their old versions, the index
    // entries, the logs.
    for i in (0..60).step_by(3) {
        for v in [
            format!("alice remembers quokka-{i:03} at the harbour"),
            format!("alice wrote zephyr-{i:03} to everyone"),
        ] {
            assert_eq!(kv.find_bytes(&needle(&v)).unwrap(), Some(0), "{v}");
        }
    }
    for g in (100..=5000).step_by(100) {
        let v = format!("alice paid kestrel-{g}");
        assert_eq!(kv.find_bytes(&needle(&v)).unwrap(), Some(0), "{v}");
    }
    // His are still found (the search is not blind), and still searchable.
    assert!(kv.find_bytes(&needle(his)).unwrap().unwrap() > 0);
    assert_eq!(
        texts(
            &mut s,
            "SELECT id FROM messages WHERE body @@ 'everyone' ORDER BY id LIMIT 2"
        ),
        ["1", "2"]
    );
    assert_eq!(
        texts(
            &mut s,
            "SELECT erasure, rows, scrubbed, found FROM nc_catalog.erasures"
        ),
        ["1 79 t 0"]
    );
    assert_eq!(
        texts(
            &mut s,
            "SELECT count(*), min(line) FROM ledger WHERE id >= 4998"
        ),
        ["2 bob paid kestrel-4998"]
    );
}

#[test]
fn the_audit_log_keeps_no_statement_of_hers() {
    let mut s = Session::new(db());
    s.execute("ALTER DATABASE nucleus SET audit_statements = 'all'")
        .unwrap();
    schema(&mut s);
    s.execute("UPDATE messages SET body = 'it''s a secret, bob' WHERE id = 1")
        .unwrap();
    // (Built so that its own text, audited too, does not name her.)
    let audit = |s: &mut Session| {
        texts(
            s,
            "SELECT count(*) FROM nc_catalog.audit \
             WHERE detail LIKE '%''ali' || 'ce''%'",
        )
    };
    assert_ne!(audit(&mut s), ["0"]);
    let before = texts(&mut s, "SELECT count(*) FROM nc_catalog.audit");
    assert_eq!(texts(&mut s, "FORGET SUBJECT 'alice'"), ["1 4 t 0"]);
    // Her statements are blanked — the one that named her, the one with
    // her row's text — the records kept, the chain whole.
    assert_eq!(audit(&mut s), ["0"]);
    assert_eq!(
        texts(
            &mut s,
            "SELECT count(*) FROM nc_catalog.audit WHERE detail LIKE '%' || 'sec' || 'ret,%'"
        ),
        ["0"]
    );
    let erased = texts(
        &mut s,
        "SELECT count(*) FROM nc_catalog.audit WHERE erasure = 1 AND detail IS NULL",
    );
    assert_eq!(
        texts(&mut s, "SELECT audit_erased FROM nc_catalog.erasures"),
        erased
    );
    assert_eq!(
        texts(&mut s, "SELECT intact, problem FROM nc_catalog.audit_chain"),
        ["t NULL"]
    );
    // Bob's are kept.
    assert_eq!(
        texts(
            &mut s,
            "SELECT count(*) > 0 FROM nc_catalog.audit WHERE detail LIKE '%''plays chess''%'"
        ),
        ["t"]
    );
    let after = texts(&mut s, "SELECT count(*) FROM nc_catalog.audit");
    assert!(after[0].parse::<u64>().unwrap() > before[0].parse::<u64>().unwrap());
}
