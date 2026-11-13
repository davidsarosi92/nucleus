//! Memory stores (PLAN.md §M14.24 R1): what `REMEMBER` did, its merge
//! never reaching a row the caller may not see, `recall` under row-level
//! security, `REMEMBER` with parameters, and expired rows deleted.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::Registry;
use nucleus_sql::{Output, Session, Value};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(11)),
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

/// `REMEMBER`'s row: the id, and what was done.
fn remember(s: &mut Session, sql: &str) -> (Value, String) {
    let mut r = rows(s, sql);
    assert_eq!(r.len(), 1);
    let r = r.pop().unwrap();
    (r[0].clone(), r[1].render())
}

fn store(s: &mut Session) {
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 64); \
         CREATE MEMORY STORE notes USING h WITH (merge = 0.99); \
         SET embedding.pending = wait",
    )
    .unwrap();
}

#[test]
fn remember_says_what_it_did() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    store(&mut s);
    let (a, did) = remember(&mut s, "REMEMBER 'likes tea' FOR 'u1' IN notes");
    assert_eq!(did, "inserted");
    assert!(
        matches!(&a, Value::Ext(t, _) if t.name() == "uuid"),
        "{a:?}"
    );
    let (b, did) = remember(&mut s, "REMEMBER 'likes tea' FOR 'u1' IN notes");
    assert_eq!((b.clone(), did.as_str()), (a.clone(), "merged"));
    // The vector's turn: the same words, other punctuation.
    let (c, did) = remember(&mut s, "REMEMBER 'Likes tea!' FOR 'u1' IN notes");
    assert_eq!((c, did.as_str()), (a.clone(), "merged"));
    let (d, did) = remember(&mut s, "REMEMBER 'likes tea' FOR 'u2' IN notes");
    assert_eq!(did, "inserted");
    assert_ne!(d, a);
    let (e, did) = remember(
        &mut s,
        "REMEMBER 'likes coffee now' FOR 'u1' IN notes \
         REPLACING (SELECT id FROM notes WHERE subject = 'u1' AND valid_to IS NULL)",
    );
    assert_eq!(did, "replaced");
    assert_eq!(
        texts(
            &mut s,
            "SELECT content, seen, valid_to IS NULL FROM notes WHERE subject = 'u1' ORDER BY created"
        ),
        ["likes tea 3 f", "likes coffee now 1 t"]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT replaces FROM notes WHERE content = 'likes coffee now'"
        )[0][0],
        a
    );
    let _ = e;
    // Each is one statement: in a block that rolls back, nothing stays.
    s.execute("BEGIN; REMEMBER 'gone' FOR 'u3' IN notes; ROLLBACK")
        .unwrap();
    assert!(texts(&mut s, "SELECT content FROM notes WHERE subject = 'u3'").is_empty());
}

#[test]
fn remember_takes_parameters() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    store(&mut s);
    let stmt = nucleus_sql::parser::parse(
        "REMEMBER $1 FOR $2 IN notes AS $3 CONFIDENCE $4 EXPIRES $5 SOURCE $6",
    )
    .unwrap()
    .remove(0);
    let (params, columns) = s.describe(&stmt, None).unwrap();
    use nucleus_sql::types::DataType;
    assert_eq!(
        params[..4],
        [
            Some(DataType::Text),
            Some(DataType::Text),
            Some(DataType::Text),
            Some(DataType::Real)
        ]
    );
    assert_eq!(params[4], Some(DataType::TimestampTz));
    assert_eq!(
        columns.map(|c| c.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
        Some(vec!["id".to_string(), "action".to_string()])
    );
    let out = s
        .run(
            &stmt,
            &[
                Value::Text("prefers dark mode".into()),
                Value::Text("u9".into()),
                Value::Text("preference".into()),
                Value::Real(0.75),
                Value::Null,
                Value::Text("settings page".into()),
            ],
        )
        .unwrap();
    assert!(matches!(out, Output::Rows { .. }));
    assert_eq!(
        texts(
            &mut s,
            "SELECT subject, kind, confidence, source, expires_at IS NULL FROM notes"
        ),
        ["u9 preference 0.75 settings page t"]
    );
}

#[test]
fn a_merge_never_reaches_a_hidden_row_and_recall_sees_only_visible_ones() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    store(&mut s);
    s.execute(
        "CREATE USER app; \
         GRANT SELECT, INSERT, UPDATE ON notes TO app; \
         GRANT USAGE ON MODEL h TO app; \
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY own ON notes TO app USING (kind <> 'private') WITH CHECK (kind <> 'private')",
    )
    .unwrap();
    // The administrator keeps a private memory.
    let (private, _) = remember(
        &mut s,
        "REMEMBER 'the door code is 4711' FOR 'u1' IN notes AS 'private'",
    );
    s.execute("SET ROLE app").unwrap();
    // The same text from the application: a memory of its own, not a
    // merge into the one it may not see.
    let (mine, did) = remember(&mut s, "REMEMBER 'the door code is 4711' FOR 'u1' IN notes");
    assert_eq!(did, "inserted");
    assert_ne!(mine, private);
    assert_eq!(
        texts(
            &mut s,
            "SELECT kind FROM recall('notes', 'u1', 'door code')"
        ),
        ["fact"]
    );
    // Nor can it replace what it cannot see.
    s.execute("RESET ROLE").unwrap();
    let id = rows(&mut s, "SELECT id::text FROM notes WHERE kind = 'private'")[0][0].render();
    s.execute("SET ROLE app").unwrap();
    assert_eq!(
        code(
            &mut s,
            &format!("REMEMBER 'x' FOR 'u1' IN notes REPLACING '{id}'")
        ),
        "42704"
    );
    assert_eq!(
        code(&mut s, "REMEMBER 'x' FOR 'u1' IN notes AS 'private'"),
        "42501"
    );
    s.execute("RESET ROLE").unwrap();
    assert_eq!(
        texts(
            &mut s,
            "SELECT kind, seen, valid_to IS NULL FROM notes ORDER BY kind"
        ),
        ["fact 1 t", "private 1 t"]
    );
}

#[test]
fn expired_rows_are_deleted() {
    let kv = db();
    let reg = Registry::builtin();
    let mut s = Session::new(kv.clone());
    store(&mut s);
    s.execute(
        "REMEMBER 'a' FOR 'u1' IN notes EXPIRES now() - interval '1 hour'; \
         REMEMBER 'b' FOR 'u1' IN notes EXPIRES now() + interval '1 hour'; \
         REMEMBER 'c' FOR 'u1' IN notes; \
         CREATE TABLE events (id int PRIMARY KEY, until date); \
         INSERT INTO events VALUES (1, current_date - 1), (2, current_date + 10), (3, NULL); \
         ALTER TABLE events SET EXPIRY (until)",
    )
    .unwrap();
    let now = rows(&mut s, "SELECT now()")[0][0].clone();
    let Value::TimestampTz(now) = now else {
        panic!("{now:?}")
    };
    let deleted = nucleus_sql::expiry::expire(&kv, &reg, None, now).unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(
        texts(&mut s, "SELECT content FROM notes ORDER BY content"),
        ["b", "c"]
    );
    assert_eq!(
        texts(&mut s, "SELECT id FROM events ORDER BY id"),
        ["2", "3"]
    );
    // A day later, `b` too; without an expiry, nothing of `events`.
    s.execute("ALTER TABLE events DROP EXPIRY").unwrap();
    let later = now + 86_400_000_000 * 30;
    assert_eq!(
        nucleus_sql::expiry::expire(&kv, &reg, None, later).unwrap(),
        1
    );
    assert_eq!(texts(&mut s, "SELECT content FROM notes"), ["c"]);
    assert_eq!(texts(&mut s, "SELECT count(*) FROM events"), ["2"]);
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, &reg).unwrap();
    assert_eq!(problems, Vec::<String>::new());
}

/// A near-duplicate is merged whether or not the queue has made the
/// first memory's vector yet: in a block, the queue cannot see the rows
/// it wrote, so their vectors are pending.
#[test]
fn a_merge_does_not_wait_for_the_queue() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 64); \
         CREATE MEMORY STORE notes USING h WITH (merge = 0.99)",
    )
    .unwrap();
    s.execute("BEGIN").unwrap();
    let (a, did) = remember(&mut s, "REMEMBER 'likes tea' FOR 'u1' IN notes");
    assert_eq!(did, "inserted");
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM notes WHERE embedding IS NULL"),
        ["1"]
    );
    let (b, did) = remember(&mut s, "REMEMBER 'Likes tea!' FOR 'u1' IN notes");
    assert_eq!((b, did.as_str()), (a, "merged"));
    let (_, did) = remember(&mut s, "REMEMBER 'Likes tea!' FOR 'u2' IN notes");
    assert_eq!(did, "inserted");
    s.execute("COMMIT").unwrap();
}
