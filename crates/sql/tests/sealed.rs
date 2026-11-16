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
