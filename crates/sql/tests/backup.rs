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
        let path = Path::new(dir).join(&name);
        match fs.open(&path, OpenMode::ReadOnly) {
            Ok(f) => {
                let mut b = vec![0u8; f.size().unwrap() as usize];
                f.read_exact_at(&mut b, 0).unwrap();
                // A part: compressed (what is in the clear is looked
                // for in what it holds).
                if name.to_str().unwrap().starts_with("part-") {
                    let n = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
                    let mut out = Vec::new();
                    nucleus_kv::lz::decompress(&b[4..], n, &mut out).unwrap();
                    all.extend_from_slice(&b);
                    b = out;
                }
                all.extend(b);
            }
            // A directory: a backup of the repository.
            Err(_) => all.extend(files(p, path.to_str().unwrap())),
        }
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
    assert_eq!(f[6..], ["full", "000001-full", "0"], "{got:?}");
    // Refused: in a block, by a role not an administrator, a relative
    // path, a directory holding something else.
    assert_eq!(code(&mut s, "BEGIN; BACKUP TO '/backups/two'"), "25001");
    s.execute("ROLLBACK").unwrap();
    s.execute("CREATE USER app; SET ROLE app").unwrap();
    assert_eq!(code(&mut s, "BACKUP TO '/backups/two'"), "42501");
    s.execute("RESET ROLE").unwrap();
    assert_eq!(code(&mut s, "BACKUP TO 'backups/two'"), "22023");
    assert_eq!(code(&mut s, "BACKUP TO '/db'"), "22023");

    let restored = open(&p, "/restored");
    let r = nucleus_sql::backup::restore(&restored, s.registry(), "/backups/one").unwrap();
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
        nucleus_sql::backup::restore(&again, t.registry(), "/backups/one")
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
    let r = nucleus_sql::backup::restore(&restored, s.registry(), "/backups/before").unwrap();
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
    nucleus_sql::backup::restore(&later, s.registry(), "/backups/after").unwrap();
    let mut u = Session::new(later);
    assert_eq!(
        texts(&mut u, "SELECT content FROM notes WHERE subject = 'alice'"),
        ["came back in spring"]
    );
    // With none of the keys, a backup opens to what is no one's.
    subject_keys::set_subject_keys(Some(Arc::new(MemoryKeys::new(|b| b.fill(1)))));
    let bare = open(&p, "/bare");
    nucleus_sql::backup::restore(&bare, s.registry(), "/backups/after").unwrap();
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

/// A backup's row: its fields by name.
fn backed_up(s: &mut Session, sql: &str) -> std::collections::HashMap<&'static str, String> {
    let got = texts(s, sql);
    let f: Vec<String> = got[0].split(' ').map(str::to_string).collect();
    [
        "commit", "entries", "sealed", "omitted", "bytes", "deleted", "kind", "backup", "removed",
    ]
    .into_iter()
    .zip(f)
    .collect()
}

/// PLAN.md §M14.18 O3: a repository's backups after the first hold what
/// changed since the one before — writes, and deletes (a subject's row's
/// sealed) — and a restore to any of them is the database as it was then.
#[test]
fn incremental_backups_restore_to_each_point() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(4));
    let kv = open(&p, "/db");
    // Keys not holding on to the database: it is opened again below.
    let rng = Arc::clone(&p);
    subject_keys::set_subject_keys(Some(Arc::new(MemoryKeys::new(move |b| {
        rng.rng().fill_unpredictable(b)
    }))));
    let mut s = Session::new(Arc::clone(&kv));
    schema(&mut s);
    s.execute("INSERT INTO cities SELECT 'town ' || g, g FROM generate_series(1, 3000) g")
        .unwrap();
    let mut points = Vec::new();
    let full = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (full["kind"].as_str(), full["backup"].as_str()),
        ("full", "000001-full")
    );
    points.push(("000001-full", answers(&mut s)));

    // A few writes, a subject's row and others deleted.
    s.execute(
        "INSERT INTO cities VALUES ('Győr', 130000); \
         UPDATE cities SET people = people + 1 WHERE name = 'Szeged'; \
         DELETE FROM cities WHERE name LIKE 'town 1%'; \
         DELETE FROM accounts WHERE id = 8; \
         REMEMBER 'swims in the lake' FOR 'carol' IN notes",
    )
    .unwrap();
    let incr = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (incr["kind"].as_str(), incr["backup"].as_str()),
        ("incremental", "000002-incr")
    );
    let n = |m: &std::collections::HashMap<&str, String>, f: &str| m[f].parse::<u64>().unwrap();
    // What changed, not all: 1111 cities and an account deleted.
    assert!(n(&incr, "deleted") >= 1112, "{incr:?}");
    assert!(
        n(&incr, "entries") < n(&full, "entries") / 2,
        "{incr:?} {full:?}"
    );
    assert!(n(&incr, "sealed") >= 2, "{incr:?}");
    points.push(("000002-incr", answers(&mut s)));
    // Its files: no subject's value in the clear.
    let all = files(&p, "/repo/000002-incr");
    assert!(!holds(&all, "swims in the lake"));
    assert!(holds(&all, "Győr"));

    // Again, after a reopening (the branch kept is found again).
    drop(s);
    let kv = {
        drop(kv);
        open(&p, "/db")
    };
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("UPDATE messages SET body = 'moved' WHERE id = 3; DELETE FROM docs WHERE id = 31")
        .unwrap();
    let third = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(third["kind"], "incremental", "{third:?}");
    points.push(("000003-incr", answers(&mut s)));
    // Only the last backup's branch is kept.
    let kept: Vec<String> = kv
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .filter(|b| b.starts_with(nucleus_sql::backup::BRANCH_PREFIX))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");

    // A restore to each point is the database as it was then.
    for (i, (name, want)) in points.iter().enumerate() {
        let restored = open(&p, &format!("/restored{i}"));
        let r = nucleus_sql::backup::restore_to(
            &restored,
            s.registry(),
            "/repo",
            &nucleus_sql::backup::Until::Name(name.to_string()),
        )
        .unwrap();
        assert_eq!(r.backups, i as u64 + 1, "{r:?}");
        let mut t = Session::new(restored);
        assert_eq!(&answers(&mut t), want, "{name}");
        assert_eq!(
            texts(&mut t, "SELECT intact FROM nc_catalog.audit_chain"),
            ["t"],
            "{name}"
        );
    }
    // By commit: the second.
    let restored = open(&p, "/by_commit");
    let r = nucleus_sql::backup::restore_to(
        &restored,
        s.registry(),
        "/repo",
        &nucleus_sql::backup::Until::Commit(n(&third, "commit") - 1),
    )
    .unwrap();
    assert_eq!(r.commit, n(&incr, "commit"));

    // FORGET drops the branch kept: the next backup is a full one, and
    // the erasure finds nothing of the subject left in the files.
    let forgot = texts(&mut s, "FORGET SUBJECT 'carol'");
    assert!(forgot[0].ends_with(" t 0"), "{forgot:?}");
    let after = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (after["kind"].as_str(), after["backup"].as_str()),
        ("full", "000004-full")
    );
    // FULL starts a new chain whatever is kept.
    let forced = backed_up(&mut s, "BACKUP FULL TO '/repo'");
    assert_eq!(forced["kind"], "full");
    // A restore of the chain before the erasure leaves her out.
    let restored = open(&p, "/before_forget");
    let r = nucleus_sql::backup::restore_to(
        &restored,
        s.registry(),
        "/repo",
        &nucleus_sql::backup::Until::Name("000003-incr".into()),
    )
    .unwrap();
    assert!(r.omitted > 0, "{r:?}");
    let mut t = Session::new(restored);
    assert_eq!(
        texts(&mut t, "SELECT count(*) FROM notes WHERE subject = 'carol'"),
        ["0"]
    );
}

/// A backup that did not finish — a crash before its manifest: its
/// directory without one, its branch made — is skipped: the next one
/// follows the last finished, its number not reused, the stray branch
/// dropped, and the chain restores.
#[test]
fn an_unfinished_backup_is_skipped() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(5));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE TABLE t (id int PRIMARY KEY, v text); INSERT INTO t VALUES (1, 'one')")
        .unwrap();
    backed_up(&mut s, "BACKUP TO '/repo'");
    s.execute("INSERT INTO t VALUES (2, 'two')").unwrap();
    backed_up(&mut s, "BACKUP TO '/repo'");
    // What a crash leaves of the third: a part, no manifest; its branch.
    let kept = kv
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .find(|b| b.starts_with(nucleus_sql::backup::BRANCH_PREFIX))
        .unwrap();
    let stray = format!("{}000003", &kept[..kept.len() - 6]);
    kv.create_branch(&stray).unwrap();
    let fs = p.fs();
    fs.create_dir_all(Path::new("/repo/000003-incr")).unwrap();
    fs.open(
        Path::new("/repo/000003-incr/part-000000"),
        OpenMode::CreateNew,
    )
    .unwrap()
    .write_all_at(b"half", 0)
    .unwrap();
    s.execute("INSERT INTO t VALUES (3, 'three'); DELETE FROM t WHERE id = 1")
        .unwrap();
    let next = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (next["kind"].as_str(), next["backup"].as_str()),
        ("incremental", "000004-incr")
    );
    let names: Vec<String> = kv.branches().unwrap().into_iter().map(|b| b.name).collect();
    assert!(
        !names.contains(&stray) && !names.contains(&kept),
        "{names:?}"
    );
    let restored = open(&p, "/restored");
    let r = nucleus_sql::backup::restore(&restored, s.registry(), "/repo").unwrap();
    assert_eq!(r.backups, 3, "{r:?}");
    let mut t = Session::new(restored);
    assert_eq!(
        texts(&mut t, "SELECT id, v FROM t ORDER BY id"),
        ["2 two", "3 three"]
    );
}

/// Reads a file of the simulated platform.
fn read(p: &Arc<SimPlatform>, path: &str) -> Vec<u8> {
    let f = p.fs().open(Path::new(path), OpenMode::ReadOnly).unwrap();
    let mut b = vec![0u8; f.size().unwrap() as usize];
    f.read_exact_at(&mut b, 0).unwrap();
    b
}

/// Writes a file of the simulated platform anew.
fn write(p: &Arc<SimPlatform>, path: &str, b: &[u8]) {
    let fs = p.fs();
    let _ = fs.remove_file(Path::new(path));
    fs.open(Path::new(path), OpenMode::CreateNew)
        .unwrap()
        .write_all_at(b, 0)
        .unwrap();
}

/// PLAN.md §M14.18 O3c: with a key, a backup's parts are encrypted and
/// its manifest authenticated — nothing of the database in its files; a
/// restore needs the key, refuses another, an altered manifest, and a
/// backup made without the key; a new key starts a new chain.
#[test]
fn an_encrypted_backup_needs_its_key() {
    use nucleus_sql::backup::{BackupKey, Until, restore_to, set_backup_key};
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    struct Unset;
    impl Drop for Unset {
        fn drop(&mut self) {
            set_backup_key(None);
        }
    }
    let _unset = Unset;
    let p = Arc::new(SimPlatform::new(6));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE t (id int PRIMARY KEY, v text); \
         INSERT INTO t SELECT g, 'the town of Szeged ' || g FROM generate_series(1, 500) g",
    )
    .unwrap();
    // Unencrypted first; a key then starts a new chain.
    backed_up(&mut s, "BACKUP TO '/repo'");
    let k1 = BackupKey::parse(&"1f".repeat(32)).unwrap();
    assert!(BackupKey::parse("12ab").is_err());
    set_backup_key(Some(k1.clone()));
    let b = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (b["kind"].as_str(), b["backup"].as_str()),
        ("full", "000002-full")
    );
    s.execute("INSERT INTO t VALUES (1000, 'Győr by the river')")
        .unwrap();
    let b = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(b["kind"], "incremental");
    for dir in ["/repo/000002-full", "/repo/000003-incr"] {
        let raw = read(&p, &format!("{dir}/part-000000"));
        assert!(!holds(&raw, "Szeged") && !holds(&raw, "Győr"), "{dir}");
        let m = String::from_utf8(read(&p, &format!("{dir}/manifest"))).unwrap();
        assert!(
            m.contains(&format!("encryption chacha20-poly1305 {}\n", k1.id())),
            "{m}"
        );
        assert!(m.contains("\nmac "), "{m}");
    }
    let want = texts(&mut s, "SELECT count(*), max(v) FROM t");
    let registry = s.registry().clone();
    let restore = |dir: &str, until: Until| {
        let r = open(&p, dir);
        restore_to(&r, &registry, "/repo", &until).map(|_| r)
    };
    // With the key: the database.
    let r = restore("/r1", Until::Latest).unwrap();
    assert_eq!(
        texts(&mut Session::new(r), "SELECT count(*), max(v) FROM t"),
        want
    );
    // With the key, the unencrypted backup is refused; without, it restores.
    let e = restore("/r2", Until::Name("000001-full".into()))
        .err()
        .unwrap();
    assert!(e.message.contains("not encrypted"), "{e:?}");
    set_backup_key(None);
    restore("/r3", Until::Name("000001-full".into())).unwrap();
    let e = restore("/r4", Until::Latest).err().unwrap();
    assert!(e.message.contains(&format!("key {}", k1.id())), "{e:?}");
    // Another key.
    let k2 = BackupKey::parse(&"2e".repeat(32)).unwrap();
    set_backup_key(Some(k2.clone()));
    let e = restore("/r5", Until::Latest).err().unwrap();
    assert!(e.message.contains("not this one"), "{e:?}");
    // A backup with it: a new chain (the last was made with the other).
    let b = backed_up(&mut s, "BACKUP TO '/repo'");
    assert_eq!(
        (b["kind"].as_str(), b["backup"].as_str()),
        ("full", "000004-full")
    );
    // An altered manifest (its commit), with the right key.
    set_backup_key(Some(k1));
    let path = "/repo/000003-incr/manifest";
    let m = String::from_utf8(read(&p, path)).unwrap();
    let altered = m.replacen("\ncommit ", "\ncommit 1", 1);
    write(&p, path, altered.as_bytes());
    let e = restore("/r6", Until::Name("000003-incr".into()))
        .err()
        .unwrap();
    assert!(e.message.contains("MAC"), "{e:?}");
    write(&p, path, m.as_bytes());
    restore("/r7", Until::Name("000003-incr".into())).unwrap();
}

/// PLAN.md §M14.18 O3c: `KEEP n` keeps the last n full backups and those
/// after them, the older chains removed whole; each backup counted in the
/// metrics.
#[test]
fn keep_removes_the_older_chains() {
    use nucleus_sql::metrics::{BACKUP_FAILURES, BACKUPS};
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(7));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE TABLE t (id int PRIMARY KEY)").unwrap();
    let made = BACKUPS.get();
    for (i, sql) in [
        "BACKUP TO '/repo'",
        "BACKUP TO '/repo'",
        "BACKUP FULL TO '/repo'",
        "BACKUP TO '/repo' KEEP 2",
        "BACKUP FULL TO '/repo' KEEP 2",
    ]
    .iter()
    .enumerate()
    {
        s.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        let b = backed_up(&mut s, sql);
        let removed = if i == 4 { "2" } else { "0" };
        assert_eq!(b["removed"], removed, "{sql}: {b:?}");
    }
    assert_eq!(BACKUPS.get(), made + 5);
    let left: Vec<String> = p
        .fs()
        .list_dir(Path::new("/repo"))
        .unwrap()
        .into_iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["000003-full", "000004-incr", "000005-full"]);
    let restored = open(&p, "/restored");
    nucleus_sql::backup::restore_to(
        &restored,
        s.registry(),
        "/repo",
        &nucleus_sql::backup::Until::Name("000004-incr".into()),
    )
    .unwrap();
    assert_eq!(
        texts(&mut Session::new(restored), "SELECT count(*) FROM t"),
        ["4"]
    );
    // A failure counted.
    let failed = BACKUP_FAILURES.get();
    assert_eq!(code(&mut s, "BACKUP TO 'relative'"), "22023");
    assert_eq!(BACKUP_FAILURES.get(), failed + 1);
    assert_eq!(code(&mut s, "BACKUP TO '/repo' KEEP 0"), "42601");
}
