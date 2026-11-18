//! Tables of the LSM engine (PLAN.md §M14.28 L3): `WITH (engine = lsm)`
//! routes a table's key range — its rows and its indexes — to an LSM
//! tree behind the store, and everything SQL does with the table answers
//! as the B+tree's twin does, before and after a reopen.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn open(p: &Arc<SimPlatform>) -> Arc<Database> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
}

fn session(db: &Arc<Database>) -> Session {
    Session::new(Arc::clone(db) as Arc<dyn Kv>)
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql).remove(0).remove(0)
}

/// The LSM tree's run files in the database's directory.
fn runs(p: &SimPlatform) -> usize {
    p.fs()
        .list_dir(Path::new("/db/engines"))
        .unwrap_or_default()
        .iter()
        .filter(|n| n.to_string_lossy().starts_with("run-"))
        .count()
}

const QUERIES: &[&str] = &[
    "SELECT count(*), sum(x), min(p), max(x) FROM c_T",
    "SELECT p, count(*) FROM c_T WHERE x > 500 GROUP BY p ORDER BY p LIMIT 5",
    "SELECT id, x FROM c_T WHERE x = 77 ORDER BY id",
    "SELECT * FROM c_T WHERE id BETWEEN 100 AND 110 ORDER BY id",
    "SELECT p.name, count(*) FROM c_T c JOIN p_T p ON p.id = c.p WHERE p.id < 4 GROUP BY p.name ORDER BY 1",
    "SELECT name FROM p_T WHERE name = 'n1!'",
    "SELECT id FROM c_T ORDER BY id DESC LIMIT 3",
];

fn same(s: &mut Session) {
    for q in QUERIES {
        assert_eq!(
            rows(s, &q.replace("_T", "_lsm")),
            rows(s, &q.replace("_T", "_btree")),
            "{q}"
        );
    }
}

#[test]
fn an_lsm_table_answers_as_its_b_tree_twin() {
    let p = Arc::new(SimPlatform::new(7));
    let db = open(&p);
    let mut s = session(&db);
    for engine in ["lsm", "btree"] {
        s.execute(&format!(
            "CREATE TABLE p_{engine} (id INT PRIMARY KEY, name TEXT UNIQUE) WITH (engine = {engine}); \
             CREATE TABLE c_{engine} (id INT PRIMARY KEY, p INT REFERENCES p_{engine} (id), x BIGINT) \
               WITH (engine = {engine}); \
             CREATE INDEX ON c_{engine} (x); \
             INSERT INTO p_{engine} SELECT i, 'n' || i FROM generate_series(1, 300) i; \
             INSERT INTO c_{engine} SELECT i, 1 + i % 300, i * 7 % 1000 FROM generate_series(1, 20000) i; \
             UPDATE c_{engine} SET x = x + 1 WHERE id % 10 = 0; \
             DELETE FROM c_{engine} WHERE id % 7 = 0; \
             UPDATE p_{engine} SET name = name || '!' WHERE id < 5"
        ))
        .unwrap();
    }
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, engine FROM nc_catalog.tables WHERE name LIKE '%_lsm' OR name LIKE '%_btree' ORDER BY name"
        ),
        [
            ["c_btree", "btree"],
            ["c_lsm", "lsm"],
            ["p_btree", "btree"],
            ["p_lsm", "lsm"]
        ]
    );
    same(&mut s);
    // Refused writes leave the LSM table as they leave the B+tree's.
    for engine in ["lsm", "btree"] {
        assert!(
            s.execute(&format!("INSERT INTO p_{engine} VALUES (1, 'again')"))
                .is_err()
        );
        assert!(
            s.execute(&format!("INSERT INTO c_{engine} VALUES (1, 999, 0)"))
                .is_err()
        );
    }
    same(&mut s);
    // A checkpoint saves the LSM tree's runs; a reopen reads them back.
    db.checkpoint().unwrap();
    assert!(runs(&p) > 0, "the LSM tables' keys are in run files");
    drop(s);
    drop(db);
    let db = open(&p);
    let mut s = session(&db);
    same(&mut s);
    // Their versions too: a rolled-back change and a deleted table.
    s.execute("BEGIN; DELETE FROM c_lsm; ROLLBACK").unwrap();
    s.execute("DELETE FROM c_lsm; DELETE FROM c_btree; DELETE FROM p_lsm; DELETE FROM p_btree")
        .unwrap();
    same(&mut s);
    assert_eq!(one(&mut s, "SELECT count(*) FROM c_lsm"), "0");
}

#[test]
fn table_ids_go_to_their_engine() {
    let p = Arc::new(SimPlatform::new(8));
    let db = open(&p);
    let mut s = session(&db);
    let id = |s: &mut Session, t: &str| -> i64 {
        one(
            s,
            &format!("SELECT table_id FROM nc_catalog.tables WHERE name = '{t}'"),
        )
        .parse()
        .unwrap()
    };
    // The id a table would take now.
    s.execute("BEGIN; CREATE TABLE probe (a INT)").unwrap();
    let first = id(&mut s, "probe");
    s.execute("ROLLBACK").unwrap();
    // A rolled-back LSM table leaves its id's range routed (a route is
    // the store's, made at once): a B+tree table passes the id over.
    s.execute("BEGIN; CREATE TABLE gone (a INT) WITH (engine = lsm); ROLLBACK")
        .unwrap();
    s.execute("CREATE TABLE b1 (a INT)").unwrap();
    s.execute("CREATE TABLE l1 (a INT) WITH (engine = lsm)")
        .unwrap();
    assert_eq!(id(&mut s, "b1"), first + 1, "b1 passed over the routed id");
    assert_eq!(id(&mut s, "l1"), first + 2);
    // The setting picks the engine of a table that names none; a
    // partition takes its parent's unless it names one.
    s.execute(
        "SET default_table_engine = lsm; CREATE TABLE l2 (a INT); RESET default_table_engine",
    )
    .unwrap();
    s.execute(
        "CREATE TABLE parts (k INT, v TEXT) PARTITION BY RANGE (k) WITH (engine = lsm); \
         CREATE TABLE parts_1 PARTITION OF parts FOR VALUES FROM (0) TO (100); \
         CREATE TABLE parts_2 PARTITION OF parts FOR VALUES FROM (100) TO (200) WITH (engine = btree); \
         INSERT INTO parts SELECT i, 'v' || i FROM generate_series(0, 199) i; \
         CREATE TABLE copied WITH (engine = lsm) AS SELECT * FROM parts WHERE k % 3 = 0",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, engine FROM nc_catalog.tables WHERE name IN ('l2', 'parts_1', 'parts_2', 'copied') ORDER BY name"
        ),
        [
            ["copied", "lsm"],
            ["l2", "lsm"],
            ["parts_1", "lsm"],
            ["parts_2", "btree"]
        ]
    );
    assert_eq!(one(&mut s, "SELECT count(*) FROM copied"), "67");
    assert!(
        s.execute("CREATE TABLE bad (a INT) WITH (engine = heap)")
            .is_err(),
        "an engine there is not"
    );
    drop(s);
    drop(db);
    let db = open(&p);
    let mut s = session(&db);
    assert_eq!(one(&mut s, "SELECT sum(k) FROM parts"), "19900");
    assert_eq!(one(&mut s, "SELECT count(*) FROM copied"), "67");
}

/// The problems the consistency check finds (none, for a test to pass).
fn problems(db: &Arc<Database>, s: &Session) -> Vec<String> {
    let txn = db.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

fn code(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(_) => "ok".into(),
        Err(e) => e.code.to_string(),
    }
}

fn engine_of(s: &mut Session, t: &str) -> String {
    one(
        s,
        &format!("SELECT engine FROM nc_catalog.tables WHERE name = '{t}'"),
    )
}

const MOVED: &[&str] = &[
    "SELECT count(*), sum(x), min(p), max(x) FROM c",
    "SELECT id, x FROM c WHERE x = 77 ORDER BY id",
    "SELECT * FROM c WHERE id BETWEEN 100 AND 110 ORDER BY id",
    "SELECT p.name, count(*) FROM c JOIN p ON p.id = c.p WHERE p.id < 4 GROUP BY 1 ORDER BY 1",
    "SELECT name FROM p WHERE name = 'n1!'",
    "SELECT count(*), sum(a) FROM h",
    "SELECT g, count(*), sum(v) FROM col GROUP BY g ORDER BY g",
    "SELECT g, count(*), sum(v) FROM both_ GROUP BY g ORDER BY g",
    "SELECT id FROM docs WHERE body @@ 'fox' ORDER BY id",
    "SELECT id FROM vecs ORDER BY v <-> '[3, 3]' LIMIT 3",
];

fn answers(s: &mut Session) -> Vec<Vec<Vec<String>>> {
    MOVED.iter().map(|q| rows(s, q)).collect()
}

/// `ALTER TABLE … SET (engine = …)` (PLAN.md §M14.28 L3b): every kind of
/// table — rows with secondary and unique indexes and foreign keys, a
/// hidden row id, columns, both copies, text and vector indexes — moves
/// to the LSM tree and back with the same answers, its old keys gone,
/// its next rows numbered on.
#[test]
fn a_table_changes_its_engine_and_back() {
    let p = Arc::new(SimPlatform::new(9));
    let db = open(&p);
    let mut s = session(&db);
    s.execute(
        "CREATE TABLE p (id INT PRIMARY KEY, name TEXT UNIQUE); \
         CREATE TABLE c (id INT PRIMARY KEY, p INT REFERENCES p (id), x BIGINT); \
         CREATE INDEX ON c (x); \
         INSERT INTO p SELECT i, 'n' || i FROM generate_series(1, 300) i; \
         INSERT INTO c SELECT i, 1 + i % 300, i * 7 % 1000 FROM generate_series(1, 20000) i; \
         UPDATE p SET name = name || '!' WHERE id < 5; \
         CREATE TABLE h (a INT); \
         INSERT INTO h SELECT i FROM generate_series(1, 500) i; \
         CREATE TABLE col (g INT, v BIGINT) WITH (storage = column); \
         INSERT INTO col SELECT i % 7, i FROM generate_series(1, 30000) i; \
         CREATE TABLE both_ (id INT PRIMARY KEY, g INT, v BIGINT); \
         INSERT INTO both_ SELECT i, i % 5, i * 3 FROM generate_series(1, 20000) i; \
         ALTER TABLE both_ SET STORAGE row, column; \
         CREATE TABLE docs (id INT PRIMARY KEY, body TEXT); \
         INSERT INTO docs VALUES (1, 'the quick brown fox'), (2, 'a lazy dog'), (3, 'fox and dog'); \
         CREATE INDEX docs_body ON docs USING text (body); \
         CREATE TABLE vecs (id INT PRIMARY KEY, v vector(2)); \
         INSERT INTO vecs SELECT i, ('[' || i || ', ' || i || ']')::vector FROM generate_series(1, 50) i; \
         CREATE INDEX vecs_v ON vecs USING hnsw (v); \
         ANALYZE",
    )
    .unwrap();
    let before = answers(&mut s);
    let tables = ["p", "c", "h", "col", "both_", "docs", "vecs"];
    for engine in ["lsm", "btree", "lsm"] {
        for t in tables {
            s.execute(&format!("ALTER TABLE {t} SET (engine = {engine})"))
                .unwrap();
            assert_eq!(engine_of(&mut s, t), engine, "{t}");
        }
        assert_eq!(answers(&mut s), before, "{engine}");
        assert_eq!(problems(&db, &s), Vec::<String>::new(), "{engine}");
    }
    // Already there: nothing to do.
    s.execute("ALTER TABLE c SET (engine = lsm)").unwrap();
    // Written on after the moves: constraints, the hidden row id's
    // numbering, the indexes.
    assert_eq!(code(&mut s, "INSERT INTO p VALUES (1, 'again')"), "23505");
    assert_eq!(code(&mut s, "INSERT INTO c VALUES (0, 999, 0)"), "23503");
    s.execute(
        "INSERT INTO h SELECT i FROM generate_series(501, 600) i; \
         INSERT INTO c VALUES (0, 1, 77); \
         INSERT INTO docs VALUES (4, 'one more fox'); \
         INSERT INTO col SELECT 1, 1 FROM generate_series(1, 10) i",
    )
    .unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM h"), "600");
    assert_eq!(one(&mut s, "SELECT sum(a) FROM h"), "180300");
    assert_eq!(
        rows(&mut s, "SELECT id FROM c WHERE x = 77 ORDER BY id LIMIT 2"),
        [["0"], ["11"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM docs WHERE body @@ 'fox' ORDER BY id"
        ),
        [["1"], ["3"], ["4"]]
    );
    assert_eq!(problems(&db, &s), Vec::<String>::new());
    // Kept across a reopen: the definitions and the LSM tree's runs.
    db.checkpoint().unwrap();
    drop(s);
    drop(db);
    let db = open(&p);
    let mut s = session(&db);
    assert_eq!(engine_of(&mut s, "both_"), "lsm");
    assert_eq!(one(&mut s, "SELECT count(*) FROM h"), "600");
    assert_eq!(problems(&db, &s), Vec::<String>::new());
}

/// Writers go on while a table moves: every write lands in the moved
/// table, whether it committed before the copy reached its key, after,
/// or across the switch.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn writes_while_a_table_moves_are_kept() {
    for (seed, storage) in [(10, "row"), (13, "column"), (14, "row, column")] {
        writes_while_moving(seed, storage);
    }
}

fn writes_while_moving(seed: u64, storage: &str) {
    let p = Arc::new(SimPlatform::new(seed));
    let db = open(&p);
    let mut s = session(&db);
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, x BIGINT, k INT); \
         CREATE INDEX ON t (k); \
         INSERT INTO t SELECT i, 0, i % 100 FROM generate_series(0, 39999) i",
    )
    .unwrap();
    if storage != "row" {
        s.execute(&format!("ALTER TABLE t SET STORAGE {storage}"))
            .unwrap();
    }
    const WRITERS: i64 = 4;
    let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            let written = Arc::clone(&written);
            std::thread::spawn(move || {
                let mut s = session(&db);
                // What this writer's rows (id % WRITERS = w) hold.
                let mut model: std::collections::BTreeMap<i64, i64> = (0..40000)
                    .filter(|i| i % WRITERS == w)
                    .map(|i| (i, 0))
                    .collect();
                let mut n: i64 = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) || n < 200 {
                    n += 1;
                    let id = (n * 7919 * WRITERS + w) % 48000;
                    let id = id - id % WRITERS + w;
                    let (sql, after) = match n % 3 {
                        0 if model.contains_key(&id) => {
                            (format!("DELETE FROM t WHERE id = {id}"), None)
                        }
                        _ if model.contains_key(&id) => (
                            format!("UPDATE t SET x = x + {n}, k = {} WHERE id = {id}", n % 100),
                            Some(model[&id] + n),
                        ),
                        _ => (
                            format!("INSERT INTO t VALUES ({id}, {n}, {})", n % 100),
                            Some(n),
                        ),
                    };
                    match s.execute(&sql) {
                        Ok(_) => match after.inspect(|_| {
                            written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }) {
                            Some(x) => {
                                model.insert(id, x);
                            }
                            None => {
                                model.remove(&id);
                            }
                        },
                        Err(e) if e.code == "40001" => {}
                        Err(e) => panic!("{sql}: {e:?}"),
                    }
                }
                (w, model)
            })
        })
        .collect();
    for engine in ["lsm", "btree"] {
        let before = written.load(std::sync::atomic::Ordering::Relaxed);
        s.execute(&format!("ALTER TABLE t SET (engine = {engine})"))
            .unwrap();
        assert_eq!(engine_of(&mut s, "t"), engine);
        assert!(
            written.load(std::sync::atomic::Ordering::Relaxed) > before,
            "{storage}: writes went on while it moved"
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in writers {
        let (w, model) = h.join().unwrap();
        let got: Vec<(i64, i64)> = rows(
            &mut s,
            &format!("SELECT id, x FROM t WHERE id % {WRITERS} = {w} ORDER BY id"),
        )
        .into_iter()
        .map(|r| (r[0].parse().unwrap(), r[1].parse().unwrap()))
        .collect();
        assert_eq!(
            got,
            model.into_iter().collect::<Vec<_>>(),
            "{storage}: writer {w}"
        );
    }
    assert_eq!(problems(&db, &s), Vec::<String>::new(), "{storage}");
}

/// A column copy made before its directory was keyed relative to the
/// table (record version 23) holds whole row keys, which name the
/// table's id: a move rewrites the directory first.
#[test]
fn an_old_column_directory_is_rewritten_before_a_move() {
    let p = Arc::new(SimPlatform::new(15));
    let db = open(&p);
    let mut s = session(&db);
    s.execute("CREATE TABLE col (id INT PRIMARY KEY, g INT, v BIGINT) WITH (storage = column)")
        .unwrap();
    // As a table made before: its directory's keys whole.
    let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
    let mut t = nucleus_sql::catalog::table(&*txn, "col").unwrap();
    t.relative_directory = false;
    nucleus_sql::catalog::update_table(&mut *txn, &t).unwrap();
    txn.commit().unwrap();
    s.execute(
        "INSERT INTO col SELECT i, i % 9, i * 2 FROM generate_series(1, 200000) i; \
         DELETE FROM col WHERE id % 11 = 0; \
         UPDATE col SET v = 0 WHERE id % 13 = 0",
    )
    .unwrap();
    let q = "SELECT g, count(*), sum(v) FROM col WHERE id > 1000 GROUP BY g ORDER BY g";
    let before = rows(&mut s, q);
    let point = rows(
        &mut s,
        "SELECT * FROM col WHERE id IN (5, 13, 22, 199999) ORDER BY id",
    );
    s.execute("ALTER TABLE col SET (engine = lsm)").unwrap();
    assert_eq!(rows(&mut s, q), before);
    assert_eq!(
        rows(
            &mut s,
            "SELECT * FROM col WHERE id IN (5, 13, 22, 199999) ORDER BY id"
        ),
        point
    );
    s.execute("INSERT INTO col VALUES (0, 0, 0)").unwrap();
    assert_eq!(code(&mut s, "INSERT INTO col VALUES (5, 0, 0)"), "23505");
    assert_eq!(problems(&db, &s), Vec::<String>::new());
}

/// A move cut short (the process gone mid-copy) leaves the table where
/// it was, its copy stale; the next move removes the copy first, as
/// `DROP TABLE` does.
#[test]
fn a_move_cut_short_is_cleaned_up() {
    let p = Arc::new(SimPlatform::new(11));
    let db = open(&p);
    let mut s = session(&db);
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO t SELECT i, 'v' || i FROM generate_series(1, 1000) i; \
         CREATE TABLE u (id INT PRIMARY KEY); \
         INSERT INTO u VALUES (1)",
    )
    .unwrap();
    let id_of = |s: &mut Session, t: &str| -> u32 {
        one(
            s,
            &format!("SELECT table_id FROM nc_catalog.tables WHERE name = '{t}'"),
        )
        .parse()
        .unwrap()
    };
    // What a crash mid-copy leaves: the record of the move and part of
    // the copy, under ids no table has.
    let stale = |db: &Arc<Database>, from: u32, to: u32| {
        let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
        nucleus_sql::catalog::set_moving(&mut *txn, from, Some(to)).unwrap();
        for p in nucleus_sql::catalog::id_prefixes(to) {
            txn.put(&[p.as_slice(), b"\x00\x00\x00\x00junk"].concat(), b"x")
                .unwrap();
        }
        txn.commit().unwrap();
    };
    let keys_under = |db: &Arc<Database>, id: u32| -> usize {
        let txn = db.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
        nucleus_sql::catalog::id_prefixes(id)
            .iter()
            .map(|p| {
                let end = nucleus_kv::prefix_end(p).unwrap();
                txn.scan(
                    std::ops::Bound::Included(p.as_slice()),
                    std::ops::Bound::Excluded(end.as_slice()),
                )
                .unwrap()
                .count()
            })
            .sum()
    };
    let (t, u) = (id_of(&mut s, "t"), id_of(&mut s, "u"));
    stale(&db, t, 7001);
    stale(&db, u, 7002);
    assert_eq!(problems(&db, &s), Vec::<String>::new(), "a copy is known");
    assert_eq!(keys_under(&db, 7001), 5);
    s.execute("ALTER TABLE t SET (engine = lsm)").unwrap();
    assert_eq!(keys_under(&db, 7001), 0);
    assert_eq!(keys_under(&db, t), 0, "the old keys went");
    assert_eq!(one(&mut s, "SELECT count(*) FROM t"), "1000");
    s.execute("DROP TABLE u").unwrap();
    assert_eq!(keys_under(&db, 7002), 0);
    assert_eq!(problems(&db, &s), Vec::<String>::new());
}

/// A partitioned table moves its partitions, and its next ones follow
/// it; what cannot move is refused.
#[test]
fn partitions_move_and_refusals() {
    let p = Arc::new(SimPlatform::new(12));
    let db = open(&p);
    let mut s = session(&db);
    s.execute(
        "CREATE TABLE parts (k INT, v TEXT) PARTITION BY RANGE (k); \
         CREATE TABLE parts_1 PARTITION OF parts FOR VALUES FROM (0) TO (100); \
         CREATE TABLE parts_2 PARTITION OF parts FOR VALUES FROM (100) TO (200); \
         INSERT INTO parts SELECT i, 'v' || i FROM generate_series(0, 199) i; \
         ALTER TABLE parts SET (engine = lsm); \
         CREATE TABLE parts_3 PARTITION OF parts FOR VALUES FROM (200) TO (300); \
         INSERT INTO parts VALUES (250, 'x')",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, engine FROM nc_catalog.tables WHERE name LIKE 'parts%' ORDER BY name"
        ),
        [
            ["parts", "lsm"],
            ["parts_1", "lsm"],
            ["parts_2", "lsm"],
            ["parts_3", "lsm"]
        ]
    );
    assert_eq!(one(&mut s, "SELECT sum(k) FROM parts"), "20150");
    assert_eq!(
        code(&mut s, "ALTER TABLE parts SET (engine = heap)"),
        "22023"
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE parts SET (fillfactor = 70)"),
        "22023"
    );
    assert_eq!(
        code(&mut s, "BEGIN; ALTER TABLE parts_1 SET (engine = btree)"),
        "25001"
    );
    s.execute("ROLLBACK").unwrap();
    assert_eq!(
        code(&mut s, "ALTER TABLE nothing SET (engine = lsm)"),
        "42P01"
    );
    assert_eq!(problems(&db, &s), Vec::<String>::new());
}
