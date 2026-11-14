//! Backups and per-subject keys (PLAN.md §M14.24 R6): `BACKUP TO` writes
//! a snapshot without the derived index pages, a restore rebuilds them;
//! each entry about a subject is sealed under its subject's key, which
//! `FORGET SUBJECT` destroys — so a restore of a backup taken before
//! leaves the subject out.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{OpenMode, Platform};
use nucleus_sql::subject_keys::{self, MemoryKeys};
use nucleus_sql::{Output, Session, Value};

/// The store of subject keys is the process's: one test at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn open(p: &Arc<SimPlatform>, dir: &str) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::clone(p) as Arc<dyn Platform>,
            Path::new(dir),
            Options {
                history_time: std::time::Duration::from_secs(3600),
                ..Options::default()
            },
        )
        .unwrap(),
    )
}

/// A fresh store of keys for `kv`'s process.
fn keys(kv: &Arc<dyn Kv>) -> Arc<MemoryKeys> {
    let k = Arc::clone(kv);
    let store = Arc::new(MemoryKeys::new(move |b| k.random_bytes(b)));
    subject_keys::set_subject_keys(Some(store.clone()));
    store
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

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

/// A memory store; messages between subjects, with attachments (no
/// subject of their own: they go with their message), and a note on each
/// message kept when it goes (its key set to NULL); accounts keyed by
/// number; documents with chunks; a table of no one.
fn schema(s: &mut Session) {
    s.execute(
        "ALTER DATABASE nucleus SET audit_statements = 'all'; \
         CREATE MODEL h USING hashing WITH (dimensions = 16); \
         CREATE MEMORY STORE notes USING h; \
         SET embedding.pending = wait; \
         CREATE TABLE messages (id int PRIMARY KEY, sender text, recipient text, body text); \
         ALTER TABLE messages SET SUBJECT (sender, recipient); \
         CREATE INDEX messages_body ON messages (body); \
         CREATE TABLE attachments (id int PRIMARY KEY, \
             message int REFERENCES messages ON DELETE CASCADE, data text); \
         CREATE TABLE flags (id int PRIMARY KEY, \
             message int REFERENCES messages ON DELETE SET NULL, why text); \
         CREATE TABLE accounts (id bigint PRIMARY KEY, name text); \
         ALTER TABLE accounts SET SUBJECT (id); \
         CREATE TABLE docs (id int PRIMARY KEY, owner text, body text); \
         ALTER TABLE docs SET SUBJECT (owner); \
         CREATE TABLE doc_chunks CHUNKS OF docs (body) BY markdown SIZE 40; \
         CREATE TABLE cities (name text PRIMARY KEY, people int); \
         CREATE INDEX cities_people ON cities (people); \
         REMEMBER 'likes green tea at the harbour' FOR 'alice' IN notes; \
         REMEMBER 'lives in Szeged by the river' FOR 'alice' IN notes; \
         REMEMBER 'plays chess on sundays' FOR 'bob' IN notes; \
         INSERT INTO messages VALUES (1, 'alice', 'bob', 'meet me at the lighthouse'), \
             (2, 'bob', 'alice', 'the lighthouse is closed'), (3, 'bob', 'carol', 'hello carol dear'); \
         INSERT INTO attachments VALUES (10, 1, 'photo of the lighthouse'), (11, 3, 'notes for carol'); \
         INSERT INTO flags VALUES (20, 1, 'spam?'), (21, 3, 'fine'); \
         INSERT INTO accounts VALUES (7, 'seven'), (8, 'eight'); \
         INSERT INTO docs VALUES (30, 'alice', '# Diary of alice\n\nToday I walked along the quay.'), \
             (31, 'bob', '# Ledger\n\nThree chess sets sold.'), \
             (32, 'alice', '# Long\n\n' || repeat('the long walk along the quay. ', 200)); \
         INSERT INTO messages SELECT g, 'bob', 'dave', 'note ' || g FROM generate_series(100, 399) g; \
         INSERT INTO cities VALUES ('Szeged', 160000), ('Pécs', 140000); \
         ANALYZE",
    )
    .unwrap();
}

/// What the files of `dir` hold, together.
fn files(p: &Arc<SimPlatform>, dir: &str) -> Vec<u8> {
    let fs = p.fs();
    let mut all = Vec::new();
    for name in fs.list_dir(Path::new(dir)).unwrap() {
        let path = Path::new(dir).join(name);
        let f = fs.open(&path, OpenMode::ReadOnly).unwrap();
        let mut b = vec![0u8; f.size().unwrap() as usize];
        f.read_exact_at(&mut b, 0).unwrap();
        all.extend(b);
    }
    all
}

fn holds(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// What the restore is compared by.
const QUERIES: [&str; 10] = [
    "SELECT subject, content FROM notes ORDER BY content",
    "SELECT id, sender, recipient, body FROM messages WHERE id < 100 ORDER BY id",
    "SELECT count(*), sum(id) FROM messages",
    "SELECT id, message, data FROM attachments ORDER BY id",
    "SELECT id, message, why FROM flags ORDER BY id",
    "SELECT id, name FROM accounts ORDER BY id",
    "SELECT id, position, chunk FROM doc_chunks ORDER BY id, position",
    "SELECT name, people FROM cities ORDER BY name",
    "SELECT id FROM messages WHERE body = 'hello carol dear'",
    "SELECT content FROM recall('notes', 'bob', 'chess')",
];

fn answers(s: &mut Session) -> Vec<Vec<String>> {
    QUERIES.iter().map(|q| texts(s, q)).collect()
}

#[test]
fn a_restore_is_the_database_as_the_backup_saw_it() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(1));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(kv);
    schema(&mut s);
    let before = answers(&mut s);
    let got = texts(&mut s, "BACKUP TO '/backups/one'");
    let f: Vec<&str> = got[0].split(' ').collect();
    // commit, entries, sealed, omitted, bytes.
    assert!(f[2].parse::<u64>().unwrap() > 10, "{got:?}");
    assert_eq!(f[3], "0");
    // Writes after the backup are not in it.
    s.execute("INSERT INTO cities VALUES ('Győr', 130000)")
        .unwrap();
    // Refused: in a block, by a role not an administrator, a relative
    // path, a directory not empty.
    assert_eq!(code(&mut s, "BEGIN; BACKUP TO '/backups/two'"), "25001");
    s.execute("ROLLBACK").unwrap();
    s.execute("CREATE USER app; SET ROLE app").unwrap();
    assert_eq!(code(&mut s, "BACKUP TO '/backups/two'"), "42501");
    s.execute("RESET ROLE").unwrap();
    assert_eq!(code(&mut s, "BACKUP TO 'backups/two'"), "22023");
    assert_eq!(code(&mut s, "BACKUP TO '/backups/one'"), "22023");

    let restored = open(&p, "/restored");
    let r =
        nucleus_sql::backup::restore(&restored, s.registry(), Path::new("/backups/one")).unwrap();
    assert_eq!((r.omitted, r.blanked, r.nulled), (0, 0, 0), "{r:?}");
    assert!(r.indexes >= 5 && r.opened > 10, "{r:?}");
    let again = Arc::clone(&restored);
    let mut t = Session::new(restored);
    assert_eq!(answers(&mut t), before);
    // The indexes are used, and whole.
    let plan = texts(
        &mut t,
        "EXPLAIN SELECT id FROM messages WHERE body = 'hello carol dear'",
    )
    .join("\n");
    assert!(plan.contains("messages_body"), "{plan}");
    // The statistics taken again; the audit log's chain holds.
    assert_ne!(
        texts(&mut t, "SELECT count(*) FROM nc_catalog.statistics"),
        ["0"]
    );
    assert_eq!(
        texts(&mut t, "SELECT intact FROM nc_catalog.audit_chain"),
        ["t"]
    );
    // A restore goes into a new database only.
    assert_eq!(
        nucleus_sql::backup::restore(&again, t.registry(), Path::new("/backups/one"))
            .unwrap_err()
            .code
            .to_string(),
        "55000"
    );
}

#[test]
fn a_backup_holds_no_value_of_a_subject_in_the_clear() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(2));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(kv);
    schema(&mut s);
    texts(&mut s, "BACKUP TO '/backups/b'");
    let all = files(&p, "/backups/b");
    // Rows of subjects, their chunks and attachments, the statements that
    // wrote them: sealed.
    for secret in [
        "likes green tea at the harbour",
        "plays chess on sundays",
        "meet me at the lighthouse",
        "photo of the lighthouse",
        "walked along the quay",
        "the long walk along the quay",
        "Three chess sets sold",
        "'alice'",
        "'carol'",
    ] {
        assert!(!holds(&all, secret), "{secret} in the clear");
    }
    // What is no one's is as it was: a row of a table of no one, a note
    // kept when its message goes.
    assert!(holds(&all, "Szeged") && holds(&all, "spam?"));
    // Without a store of keys, no backup of subjects.
    subject_keys::set_subject_keys(None);
    assert_eq!(code(&mut s, "BACKUP TO '/backups/c'"), "55000");
}

#[test]
fn a_restore_after_forget_leaves_the_subject_out() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(3));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(kv);
    schema(&mut s);
    texts(&mut s, "BACKUP TO '/backups/before'");
    // After the backup: her rows go, and with them her key.
    texts(&mut s, "FORGET SUBJECT 'alice'");
    let after = answers(&mut s);
    // (Her new rows after the erasure have a key of their own.)
    s.execute("REMEMBER 'came back in spring' FOR 'alice' IN notes")
        .unwrap();
    texts(&mut s, "BACKUP TO '/backups/after'");

    let restored = open(&p, "/restored");
    let r = nucleus_sql::backup::restore(&restored, s.registry(), Path::new("/backups/before"))
        .unwrap();
    // Two notes, two messages, an attachment, a document and its chunks,
    // their embedding vectors…
    assert!(r.omitted >= 7, "{r:?}");
    // The statements naming her, blanked; the note on her message kept,
    // its message gone.
    assert!(r.blanked > 0, "{r:?}");
    assert_eq!(r.nulled, 1, "{r:?}");
    let mut t = Session::new(restored);
    // As the database is now, but for what came after the backup.
    assert_eq!(answers(&mut t), after);
    assert_eq!(
        texts(
            &mut t,
            "SELECT count(*) FROM nc_catalog.audit WHERE detail LIKE '%alice%'"
        ),
        ["0"]
    );
    assert_eq!(
        texts(&mut t, "SELECT intact FROM nc_catalog.audit_chain"),
        ["t"]
    );
    // A restore of the later backup has her new note, and only that.
    let later = open(&p, "/later");
    nucleus_sql::backup::restore(&later, s.registry(), Path::new("/backups/after")).unwrap();
    let mut u = Session::new(later);
    assert_eq!(
        texts(&mut u, "SELECT content FROM notes WHERE subject = 'alice'"),
        ["came back in spring"]
    );
    // With none of the keys, a backup opens to what is no one's.
    subject_keys::set_subject_keys(Some(Arc::new(MemoryKeys::new(|b| b.fill(1)))));
    let bare = open(&p, "/bare");
    nucleus_sql::backup::restore(&bare, s.registry(), Path::new("/backups/after")).unwrap();
    let mut v = Session::new(bare);
    assert_eq!(
        texts(
            &mut v,
            "SELECT (SELECT count(*) FROM notes), (SELECT count(*) FROM messages), \
             (SELECT count(*) FROM cities), (SELECT count(*) FROM flags WHERE message IS NULL)"
        ),
        ["0 0 2 2"]
    );
}
