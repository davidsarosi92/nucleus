//! Sealed rows (PLAN.md §M14.30 K4a): a table stored `sealed` keeps each
//! row under its subjects' key — nothing of it in the clear in any file,
//! log or data; `FORGET SUBJECT` destroys the key, and with it every copy
//! of the subject's rows: on a branch made before, in history, in a
//! backup.  Its indexes cover only its subject columns; a table converts
//! to it and back online.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{OpenMode, Platform};
use nucleus_sql::subject_keys::{self, MemoryKeys};
use nucleus_sql::{Output, Session, Value};

/// The store of subject keys is the process's: one test at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const NEEDLE: &str = "sealed-planted-4c1e9a27";

fn open(p: &Arc<SimPlatform>, dir: &str) -> Arc<Database> {
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

fn keys(kv: &Arc<Database>) -> Arc<MemoryKeys> {
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

fn run(s: &mut Session, sql: &str) {
    s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

/// Every file under `dir`, together.
fn files(p: &Arc<SimPlatform>, dir: &str) -> Vec<u8> {
    let fs = p.fs();
    let mut all = Vec::new();
    for name in fs.list_dir(Path::new(dir)).unwrap() {
        let path = Path::new(dir).join(&name);
        match fs.open(&path, OpenMode::ReadOnly) {
            Ok(f) => {
                let mut b = vec![0u8; f.size().unwrap() as usize];
                f.read_exact_at(&mut b, 0).unwrap();
                all.extend(b);
            }
            Err(_) => all.extend(files(p, path.to_str().unwrap())),
        }
    }
    all
}

fn holds(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Notes of two people (one long enough to go past a chunk), sealed.
fn schema(s: &mut Session) {
    run(
        s,
        "CREATE TABLE notes (id int PRIMARY KEY, owner text, body text, n int) \
         WITH (storage = sealed)",
    );
    // No subject yet: nothing to seal under.
    assert_eq!(
        code(s, "INSERT INTO notes VALUES (1, 'alice', 'x', 1)"),
        "55000"
    );
    run(s, "ALTER TABLE notes SET SUBJECT (owner)");
    run(s, "CREATE INDEX notes_owner ON notes (owner)");
    run(
        s,
        &format!(
            "INSERT INTO notes SELECT i, CASE WHEN i % 2 = 0 THEN 'alice' ELSE 'bob' END, \
             '{NEEDLE} ' || i, i FROM generate_series(1, 200) i"
        ),
    );
    run(
        s,
        &format!("INSERT INTO notes VALUES (1000, 'alice', repeat('{NEEDLE} long ', 6000), 0)"),
    );
}

#[test]
fn a_sealed_table_holds_nothing_in_the_clear() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(1));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    schema(&mut s);
    assert_eq!(
        texts(
            &mut s,
            "SELECT count(*), sum(n) FROM notes WHERE owner = 'alice'"
        ),
        ["101 10100"]
    );
    assert_eq!(
        texts(&mut s, "SELECT length(body) FROM notes WHERE id = 1000"),
        [(format!("{NEEDLE} long ").len() * 6000).to_string()]
    );
    assert_eq!(
        texts(&mut s, "SELECT body FROM notes WHERE id = 7"),
        [format!("{NEEDLE} 7")]
    );
    run(
        &mut s,
        "UPDATE notes SET body = body || ' again' WHERE n < 10",
    );
    run(&mut s, "DELETE FROM notes WHERE id = 3");
    assert_eq!(
        texts(
            &mut s,
            "SELECT body FROM notes WHERE id IN (3, 4) ORDER BY id"
        ),
        [format!("{NEEDLE} 4 again")]
    );
    run(&mut s, "ANALYZE notes");
    assert_eq!(texts(&mut s, "SELECT count(*) FROM notes"), ["200"]);
    // In the log, and in the pages after a checkpoint: nowhere in the clear.
    assert!(!holds(&files(&p, "/db"), NEEDLE), "in the log");
    kv.checkpoint().unwrap();
    assert!(!holds(&files(&p, "/db"), NEEDLE), "in the pages");

    // Indexes on what is sealed: refused, however made.
    for sql in [
        "CREATE INDEX notes_body ON notes (body)",
        "CREATE INDEX notes_two ON notes (owner, n)",
        "CREATE INDEX notes_lower ON notes (lower(owner))",
        "CREATE INDEX notes_text ON notes USING text (body)",
        "ALTER TABLE notes ADD CONSTRAINT one_n UNIQUE (n)",
    ] {
        assert_eq!(code(&mut s, sql), "0A000", "{sql}");
    }
    // A row of no one.
    assert_eq!(
        code(&mut s, "INSERT INTO notes VALUES (5000, NULL, 'x', 1)"),
        "23502"
    );
}

#[test]
fn forgetting_leaves_no_copy_readable() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(2));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    schema(&mut s);
    let before = texts(&mut s, "SELECT snapshot_commit()")[0].clone();
    run(&mut s, "CREATE BRANCH earlier");
    texts(&mut s, "BACKUP TO '/backups/before'");
    texts(&mut s, "FORGET SUBJECT 'alice'");
    assert_eq!(
        texts(&mut s, "SELECT owner, count(*) FROM notes GROUP BY owner"),
        ["bob 100"]
    );
    // The branch made before: her rows there, unreadable — as if gone.
    let mut b = Session::new(kv.branch("earlier").unwrap());
    assert_eq!(
        texts(&mut b, "SELECT owner, count(*) FROM notes GROUP BY owner"),
        ["bob 100"]
    );
    assert_eq!(
        texts(&mut b, "SELECT count(*) FROM notes WHERE owner = 'alice'"),
        ["0"]
    );
    // Her rows there are no fault of the database's.
    let branch = kv.branch("earlier").unwrap();
    let txn = branch.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    drop(txn);
    // Opened there: her rows removed, their index entries with them.
    run(&mut b, "ALTER TABLE notes SET STORAGE row");
    assert_eq!(
        texts(&mut b, "SELECT owner, count(*) FROM notes GROUP BY owner"),
        ["bob 100"]
    );
    assert_eq!(
        texts(&mut b, "SELECT count(*) FROM notes WHERE owner = 'alice'"),
        ["0"]
    );
    let txn = branch.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    // History: the same.
    assert_eq!(
        texts(
            &mut s,
            &format!("SELECT count(*) FROM notes AS OF COMMIT {before} WHERE owner = 'alice'")
        ),
        ["0"]
    );
    // A backup taken before: restored without her.
    let restored = open(&p, "/restored");
    nucleus_sql::backup::restore(
        &(Arc::clone(&restored) as Arc<dyn Kv>),
        s.registry(),
        "/backups/before",
    )
    .unwrap();
    let mut r = Session::new(restored as Arc<dyn Kv>);
    assert_eq!(
        texts(&mut r, "SELECT owner, count(*) FROM notes GROUP BY owner"),
        ["bob 100"]
    );
    // She comes back: a key of her own anew.
    run(
        &mut s,
        "INSERT INTO notes VALUES (9000, 'alice', 'spring', 1)",
    );
    assert_eq!(
        texts(&mut s, "SELECT body FROM notes WHERE owner = 'alice'"),
        ["spring"]
    );
}

#[test]
fn a_table_is_sealed_and_opened_online() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(3));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, who text, v text, w int)",
    );
    run(&mut s, "CREATE INDEX t_w ON t (w)");
    run(
        &mut s,
        &format!(
            "INSERT INTO t SELECT i, 'p' || (i % 5), '{NEEDLE} ' || i, i FROM generate_series(1, 3000) i"
        ),
    );
    run(
        &mut s,
        &format!("INSERT INTO t VALUES (5000, 'p1', repeat('{NEEDLE} ', 9000), 0)"),
    );
    let all = "SELECT count(*), sum(w), sum(length(v)) FROM t";
    let want = texts(&mut s, all);
    // No subject, an index on a column sealed: refused.
    assert_eq!(code(&mut s, "ALTER TABLE t SET STORAGE sealed"), "55000");
    run(&mut s, "ALTER TABLE t SET SUBJECT (who)");
    assert_eq!(code(&mut s, "ALTER TABLE t SET STORAGE sealed"), "0A000");
    run(&mut s, "DROP INDEX t_w");
    run(&mut s, "ALTER TABLE t SET STORAGE sealed");
    assert_eq!(texts(&mut s, all), want);
    run(&mut s, "UPDATE t SET w = w + 1 WHERE id <= 10");
    kv.checkpoint().unwrap();
    texts(&mut s, "FORGET SUBJECT 'p1'");
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM t WHERE who = 'p1'"),
        ["0"]
    );
    // And back.
    run(&mut s, "ALTER TABLE t SET STORAGE row");
    assert_eq!(
        texts(&mut s, "SELECT count(*), sum(w) FROM t"),
        ["2400 3602408"]
    );
    run(&mut s, "CREATE INDEX t_w ON t (w)");
    assert_eq!(
        texts(&mut s, "SELECT v FROM t WHERE w = 3000"),
        [format!("{NEEDLE} 3000")]
    );
}

/// What sealing costs, against the same table stored as rows: a load, a
/// scan, lookups by key (`cargo test --release -p nucleus-sql --test
/// sealed -- --ignored --nocapture`).
#[test]
#[ignore]
fn the_cost_of_sealing() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let kv: Arc<dyn Kv> = Arc::new(Database::open_in_memory(Options::default()).unwrap());
    let k = Arc::clone(&kv);
    subject_keys::set_subject_keys(Some(Arc::new(MemoryKeys::new(move |b| k.random_bytes(b)))));
    let mut s = Session::new(kv);
    let n = 200_000;
    for storage in ["row", "sealed"] {
        run(&mut s, "DROP TABLE IF EXISTS m");
        run(
            &mut s,
            &format!(
                "CREATE TABLE m (id int PRIMARY KEY, who text, body text, v int) \
                 WITH (storage = {storage})"
            ),
        );
        run(&mut s, "ALTER TABLE m SET SUBJECT (who)");
        let t = std::time::Instant::now();
        run(
            &mut s,
            &format!(
                "INSERT INTO m SELECT i, 'user' || (i % 1000), \
                 'a memory of some length, as a model keeps one: ' || i, i \
                 FROM generate_series(1, {n}) i"
            ),
        );
        let load = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..3 {
            texts(&mut s, "SELECT sum(v), sum(length(body)) FROM m");
        }
        let scan = t.elapsed() / 3;
        let t = std::time::Instant::now();
        for i in 0..20_000 {
            texts(
                &mut s,
                &format!("SELECT body FROM m WHERE id = {}", 1 + i * 7),
            );
        }
        let get = t.elapsed() / 20_000;
        println!("{storage:>6}: load {n} rows {load:.2?}, scan {scan:.2?}, lookup {get:.2?}");
    }
}

/// K4c's measure: `recall` over a sealed memory store — every one of the
/// subject's current memories read, opened and ranked — against the
/// indexed store's HNSW and text index, a user's memories being few.
#[test]
#[ignore]
fn the_cost_of_a_sealed_memory_store() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let kv: Arc<dyn Kv> = Arc::new(Database::open_in_memory(Options::default()).unwrap());
    let k = Arc::clone(&kv);
    subject_keys::set_subject_keys(Some(Arc::new(MemoryKeys::new(move |b| k.random_bytes(b)))));
    let mut s = Session::new(kv);
    run(
        &mut s,
        "CREATE MODEL h USING hashing WITH (dimensions = 384); SET embedding.pending = wait",
    );
    let subjects = 1000;
    for per in [20, 100] {
        let n = subjects * per;
        for sealed in [false, true] {
            run(&mut s, "DROP TABLE IF EXISTS notes");
            run(
                &mut s,
                &format!("CREATE MEMORY STORE notes USING h WITH (sealed = {sealed})"),
            );
            let t = std::time::Instant::now();
            run(
                &mut s,
                &format!(
                    "INSERT INTO notes (subject, content) SELECT 'user' || (i % {subjects}), \
                     'a memory ' || i || ' of word' || (i * 7 % 500) || ' and word' || \
                     (i * 13 % 300) || ', kept as a model keeps one' \
                     FROM generate_series(1, {n}) i"
                ),
            );
            texts(&mut s, "SELECT count(*) FROM notes");
            let load = t.elapsed();
            let calls = 300;
            let t = std::time::Instant::now();
            for i in 0..calls {
                let found = texts(
                    &mut s,
                    &format!(
                        "SELECT content FROM recall('notes', 'user{}', 'word{} word{}', 10)",
                        i * 37 % subjects,
                        i * 7 % 500,
                        i % 300
                    ),
                );
                assert!(!found.is_empty());
            }
            let recall = t.elapsed() / calls as u32;
            println!(
                "{per:>4} memories a subject, {:>7}: load {n} {load:.2?}, recall {recall:.2?}",
                if sealed { "sealed" } else { "indexed" }
            );
        }
    }
}

/// What a user presents: 32 bytes only the user has.
const SECRET: [u8; 32] = [0x5a; 32];

/// A subject's key in its user's keeping (K4b): only the wrapped key is
/// kept; another session reads the subject's rows only once it presents
/// the user's secret (a wrong one refused), the others' as before; a
/// backup copies the rows sealed, a restore opens them for the user only;
/// `FORGET` needs no secret, and leaves the rows unreadable for good.
#[test]
fn a_key_its_user_holds() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(4));
    let kv = open(&p, "/db");
    let store = keys(&kv);
    let mut s = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    schema(&mut s);
    s.hold_subject_key("alice", SECRET).unwrap();
    let text = store.table().to_text();
    assert!(text.contains("\nh "), "{text}");
    // Held already.
    assert_eq!(
        s.hold_subject_key("alice", [1; 32]).unwrap_err().code,
        "22023"
    );
    // The holding session presented it.
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM notes WHERE owner = 'alice'"),
        ["101"]
    );

    let mut t = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    assert_eq!(
        texts(&mut t, "SELECT count(*) FROM notes WHERE owner = 'bob'"),
        ["100"]
    );
    for sql in [
        "SELECT count(*) FROM notes",
        "SELECT body FROM notes WHERE owner = 'alice'",
        "INSERT INTO notes VALUES (7000, 'alice', 'new', 1)",
    ] {
        assert_eq!(code(&mut t, sql), "28000", "{sql}");
    }
    assert_eq!(
        t.present_subject_key("alice", [1; 32]).unwrap_err().code,
        "28000"
    );
    t.present_subject_key("alice", SECRET).unwrap();
    run(&mut t, "INSERT INTO notes VALUES (7000, 'alice', 'new', 1)");
    assert_eq!(
        texts(&mut t, "SELECT count(*) FROM notes WHERE owner = 'alice'"),
        ["102"]
    );

    // A backup copies her rows sealed; restored, they open for her alone.
    texts(&mut s, "BACKUP TO '/backups/held'");
    let restored: Arc<dyn Kv> = open(&p, "/restored");
    nucleus_sql::backup::restore(&restored, s.registry(), "/backups/held").unwrap();
    let mut r = Session::new(Arc::clone(&restored));
    assert_eq!(code(&mut r, "SELECT count(*) FROM notes"), "28000");
    r.present_subject_key("alice", SECRET).unwrap();
    assert_eq!(
        texts(
            &mut r,
            "SELECT owner, count(*) FROM notes GROUP BY owner ORDER BY owner"
        ),
        ["alice 102", "bob 100"]
    );

    // Forgotten by a session that never had her secret.
    let mut admin = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    texts(&mut admin, "FORGET SUBJECT 'alice'");
    assert_eq!(
        texts(&mut t, "SELECT owner, count(*) FROM notes GROUP BY owner"),
        ["bob 100"]
    );
    assert_eq!(texts(&mut admin, "SELECT count(*) FROM notes"), ["100"]);
    assert!(!store.table().to_text().contains("\nh "));
    let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

/// PLAN.md §M14.30 K4c: a memory store `WITH (sealed)` — its rows sealed,
/// no HNSW or text index (they would hold the texts and vectors in the
/// clear); `REMEMBER` merges and `recall` ranks over the subject's own
/// memories exactly, BM25 by those rows' statistics.
#[test]
fn a_sealed_memory_store() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = Arc::new(SimPlatform::new(5));
    let kv = open(&p, "/db");
    keys(&kv);
    let mut s = Session::new(Arc::clone(&kv) as Arc<dyn Kv>);
    run(
        &mut s,
        "CREATE MODEL h USING hashing WITH (dimensions = 64); \
         CREATE MEMORY STORE notes USING h WITH (sealed, analyzer = english); \
         SET embedding.pending = wait",
    );
    assert_eq!(
        texts(
            &mut s,
            "SELECT name FROM nc_catalog.indexes WHERE table_name = 'notes' AND NOT is_primary"
        ),
        ["notes_subject"]
    );
    for (who, what) in [
        ("ann", format!("{NEEDLE} drinks green tea every morning")),
        ("ann", "walks the dog at noon".to_string()),
        ("ann", "reads poems about the sea".to_string()),
        ("bob", "drinks tea, black, no sugar".to_string()),
        ("bob", "fixes bicycles".to_string()),
    ] {
        run(&mut s, &format!("REMEMBER '{what}' FOR '{who}' IN notes"));
    }
    // The same text again: merged, as in any store.
    assert_eq!(
        texts(
            &mut s,
            "REMEMBER 'walks the dog at noon' FOR 'ann' IN notes"
        )[0]
        .split(' ')
        .nth(1),
        Some("merged")
    );
    // Ann's memories alone, the one about tea first; the statistics hers.
    let found = texts(
        &mut s,
        "SELECT content FROM recall('notes', 'ann', 'tea in the morning', 2)",
    );
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].contains("green tea"), "{found:?}");
    assert_eq!(
        texts(
            &mut s,
            "SELECT st[1], st[3] FROM (SELECT bm25_stats(content, 'tea', 'english') AS st \
             FROM notes WHERE subject = 'ann') x"
        ),
        ["3 1"]
    );
    // An index on the texts or vectors: refused.
    for sql in [
        "CREATE INDEX notes_text ON notes USING text (content)",
        "CREATE INDEX notes_vec ON notes USING hnsw (embedding vector_cosine_ops)",
    ] {
        assert_eq!(code(&mut s, sql), "0A000", "{sql}");
    }
    // The schema says it is sealed.
    assert_eq!(
        texts(
            &mut s,
            "SELECT definition FROM nc_catalog.definitions WHERE kind = 'memory store'"
        ),
        ["CREATE MEMORY STORE notes USING h WITH (analyzer = english, sealed = 'true')"]
    );
    kv.checkpoint().unwrap();
    assert!(!holds(&files(&p, "/db"), NEEDLE), "in the clear");
    // Forgotten: nothing of hers to recall.
    texts(&mut s, "FORGET SUBJECT 'ann'");
    assert!(texts(&mut s, "SELECT content FROM recall('notes', 'ann', 'tea')").is_empty());
    assert_eq!(
        texts(
            &mut s,
            "SELECT content FROM recall('notes', 'bob', 'tea', 1)"
        ),
        ["drinks tea, black, no sugar"]
    );
}
