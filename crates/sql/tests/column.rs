//! Columnar storage (PLAN.md §M14.20 V7) against the row storage: the
//! same table in each, the same writes — loads that make several segments
//! and merge, updates, key changes, deletes, small writes left in the
//! delta store — and after each, the same rows and query results (a row and
//! a batch at a time, in parallel), and a consistent integrity check.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session};

fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            Some(_) => Ok(Vec::new()),
            None => Ok(Vec::new()),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

fn both(s: &mut Session, sql: &str) {
    let r = outcome(s, &sql.replace("TT", "r"));
    // (Errors name the table's constraints.)
    let c = outcome(s, &sql.replace("TT", "c")).map_err(|e| e.replace("\"c_", "\"r_"));
    assert_eq!(c, r, "{sql}");
}

const QUERIES: &[&str] = &[
    "SELECT * FROM TT ORDER BY id",
    "SELECT count(*), sum(x), min(x), max(x), avg(f), min(s), max(d) FROM TT",
    "SELECT g, count(*), sum(x), max(s) FROM TT GROUP BY g ORDER BY g",
    "SELECT id, x FROM TT WHERE x BETWEEN 100 AND 200 ORDER BY id",
    "SELECT id FROM TT WHERE s = 'name 7' ORDER BY id",
    "SELECT count(*) FROM TT WHERE s LIKE 'name 1%' AND ok",
    "SELECT id, d FROM TT WHERE d >= DATE '2026-06-01' ORDER BY id LIMIT 20",
    "SELECT * FROM TT WHERE id = 777",
    "SELECT * FROM TT WHERE id BETWEEN 30000 AND 30010",
    "SELECT id FROM TT WHERE x = 5 ORDER BY id",
    "SELECT count(*) FROM TT WHERE f IS NULL",
    "SELECT * FROM TT WHERE id > 1000000",
];

fn compare(s: &mut Session) {
    for q in QUERIES {
        both(s, q);
    }
}

#[test]
fn column_tables_agree_with_row_tables() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(4)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&db));
    for (t, storage) in [("r", "row"), ("c", "column")] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, g INT, x BIGINT, f FLOAT, s TEXT, ok BOOL, d DATE) \
             WITH (storage = {storage}); CREATE INDEX ON {t} (x)"
        ))
        .unwrap();
    }
    let rng = SplitMix64::new(9);
    let load = |from: i64, to: i64| {
        format!(
            "INSERT INTO TT SELECT i, i % 17, i * 7919 % 1000, \
             CASE WHEN i % 13 = 0 THEN NULL ELSE i * 0.5 END, 'name ' || (i % 50), i % 3 = 0, \
             DATE '2026-01-01' + i % 365 FROM generate_series({from}, {to}) i"
        )
    };
    let mut steps: Vec<String> = vec![
        load(1, 40000),     // past a segment's rows: two segments
        load(40001, 41000), // left in the delta store
        "UPDATE TT SET x = x + 1, s = s || '!' WHERE id % 100 = 0".into(),
        "DELETE FROM TT WHERE id % 37 = 0".into(),
        load(80001, 90000), // a merge, appended
        "UPDATE TT SET id = id + 200000 WHERE id BETWEEN 100 AND 150".into(),
        "DELETE FROM TT WHERE id BETWEEN 39990 AND 40500".into(),
        load(41001, 45000), // a merge among existing segments
    ];
    for _ in 0..6 {
        let id = rng.below(90000) as i64 + 1;
        steps.push(match rng.below(3) {
            0 => format!(
                "UPDATE TT SET g = g + 1 WHERE id BETWEEN {id} AND {}",
                id + 50
            ),
            1 => format!("DELETE FROM TT WHERE id BETWEEN {id} AND {}", id + 20),
            _ => format!(
                "INSERT INTO TT SELECT i, 1, 2, 3.0, 'late', true, DATE '2027-01-01' \
                 FROM generate_series({}, {}) i",
                1_000_000 + id,
                1_000_000 + id + 10
            ),
        });
    }
    // A write that fails leaves both as they were.
    steps.push("INSERT INTO TT VALUES (5, 1, 1, 1, 'dup', true, DATE '2026-01-01')".into());
    for step in &steps {
        both(&mut s, step);
        compare(&mut s);
    }
    let segments = outcome(
        &mut s,
        "SELECT count(*), sum(rows) > 45000, sum(deleted) > 0 FROM nc_catalog.segments \
         WHERE table_name = 'c'",
    )
    .unwrap();
    assert!(segments[0][0].parse::<i64>().unwrap() >= 2, "{segments:?}");
    assert_eq!(segments[0][1..], ["t", "t"]);
    for batch in ["0", "3"] {
        s.execute(&format!("SET batch_size = {batch}")).unwrap();
        compare(&mut s);
    }
    s.execute("SET batch_size = 1024").unwrap();
    s.execute("SET parallel_workers = 4").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    compare(&mut s);
    assert!(
        nucleus_sql::vector::parallel_runs() > before,
        "scanned in parallel"
    );
    let txn = db.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
        Vec::<String>::new()
    );
}

/// A delete of a segment's row and a merge that rewrites the segment, in
/// transactions at once, in either order: both commit (a marker is kept by
/// the row, so the merge does not conflict with it) and the delete holds.
#[test]
fn a_delete_and_a_merge_of_one_segment_both_hold() {
    for delete_first in [true, false] {
        let db: Arc<dyn Kv> = Arc::new(
            Database::open_on(
                Arc::new(SimPlatform::new(6)),
                Path::new("/db"),
                Options::default(),
            )
            .unwrap(),
        );
        let mut setup = Session::new(Arc::clone(&db));
        setup
            .execute(
                "CREATE TABLE c (id INT PRIMARY KEY, x INT) WITH (storage = column); \
                 INSERT INTO c SELECT i * 2, i FROM generate_series(1, 6000) i",
            )
            .unwrap();
        let mut merger = Session::new(Arc::clone(&db));
        let mut deleter = Session::new(Arc::clone(&db));
        merger.execute("BEGIN").unwrap();
        deleter.execute("BEGIN").unwrap();
        // Odd keys among the segment's even ones: the merge rewrites it.
        merger
            .execute("INSERT INTO c SELECT i * 2 + 1, i FROM generate_series(1, 5000) i")
            .unwrap();
        deleter.execute("DELETE FROM c WHERE id = 100").unwrap();
        let (first, second) = if delete_first {
            (&mut deleter, &mut merger)
        } else {
            (&mut merger, &mut deleter)
        };
        first.execute("COMMIT").unwrap();
        second.execute("COMMIT").unwrap();
        let left = outcome(
            &mut setup,
            "SELECT count(*), sum(CASE WHEN id = 100 THEN 1 ELSE 0 END) FROM c",
        )
        .unwrap();
        assert_eq!(left, [["10999", "0"]], "delete first: {delete_first}");
        let txn = db.begin(TxnOptions::read_only()).unwrap();
        assert_eq!(
            nucleus_sql::check::check(&*txn, setup.registry()).unwrap(),
            Vec::<String>::new()
        );
    }
}

/// Two transactions write a column table at once — each many rows (they
/// go to segments) or few (to the delta store), their keys overlapping or
/// not — committed in either order.  Overlapping keys conflict as in a row
/// table: the second to commit fails, retryable, and none of the first's
/// rows is lost; writes to different keys keep every row they commit.
/// (Segments are listed by their last key, so two segments over the same
/// keys do not share a key to conflict on; their reads of the directory
/// and the delta store are what conflicts.)
#[test]
fn concurrent_writes_of_a_column_table_conflict_as_rows_do() {
    // (first key, rows): many rows go to segments, few to the delta store.
    let shapes: [(&str, i64, i64); 6] = [
        ("many", 1, 10_000),
        ("many, overlapping", 5_001, 10_000),
        ("many, after", 20_001, 10_000),
        ("few", 2, 10),
        ("few, inside", 4_001, 10),
        ("few, after", 50_001, 10),
    ];
    for a in 0..shapes.len() {
        for b in 0..shapes.len() {
            for a_first in [true, false] {
                let db: Arc<dyn Kv> = Arc::new(
                    Database::open_on(
                        Arc::new(SimPlatform::new(7)),
                        Path::new("/db"),
                        Options::default(),
                    )
                    .unwrap(),
                );
                let mut setup = Session::new(Arc::clone(&db));
                setup
                    .execute("CREATE TABLE c (id INT PRIMARY KEY, w INT) WITH (storage = column)")
                    .unwrap();
                let mut sa = Session::new(Arc::clone(&db));
                let mut sb = Session::new(Arc::clone(&db));
                let ins = |w: usize, (_, from, n): (&str, i64, i64)| {
                    format!(
                        "INSERT INTO c SELECT i, {w} FROM generate_series({from}, {}) i",
                        from + n - 1
                    )
                };
                sa.execute("BEGIN").unwrap();
                sb.execute("BEGIN").unwrap();
                sa.execute(&ins(1, shapes[a])).unwrap();
                sb.execute(&ins(2, shapes[b])).unwrap();
                let (first, second) = if a_first {
                    (&mut sa, &mut sb)
                } else {
                    (&mut sb, &mut sa)
                };
                let first_ok = first.execute("COMMIT").is_ok();
                let second_ok = match second.execute("COMMIT") {
                    Ok(_) => true,
                    Err(e) => {
                        assert_eq!(e.code, "40001", "{e:?}");
                        false
                    }
                };
                let what = format!("{} / {}, a first: {a_first}", shapes[a].0, shapes[b].0);
                assert!(first_ok, "{what}");
                let keys = |(_, from, n): (&str, i64, i64)| from..from + n;
                let (ka, kb) = (keys(shapes[a]), keys(shapes[b]));
                let overlap = ka.start < kb.end && kb.start < ka.end;
                if overlap {
                    assert!(!second_ok, "{what}: overlapping writes both committed");
                }
                // Few rows each, to different keys: no conflict (a write
                // to segments past the last one may conflict with another
                // there — its claim of the directory runs to the end —,
                // never one to the delta store with another).
                if !overlap && shapes[a].0.starts_with("few") && shapes[b].0.starts_with("few") {
                    assert!(
                        second_ok,
                        "{what}: small writes to different keys conflicted"
                    );
                }
                // Every committed writer's rows are there, under its mark.
                let (ok_a, ok_b) = if a_first {
                    (true, second_ok)
                } else {
                    (second_ok, true)
                };
                let expect =
                    |ok: bool, r: &std::ops::Range<i64>| if ok { r.end - r.start } else { 0 };
                let got = outcome(
                    &mut setup,
                    "SELECT count(*), count(DISTINCT id), \
                            sum(CASE WHEN w = 1 THEN 1 ELSE 0 END), \
                            sum(CASE WHEN w = 2 THEN 1 ELSE 0 END) FROM c",
                )
                .unwrap();
                let (na, nb) = (expect(ok_a, &ka), expect(ok_b, &kb));
                let total = (na + nb).to_string();
                let mark = |n: i64| {
                    if na + nb == 0 {
                        String::new()
                    } else {
                        n.to_string()
                    }
                };
                assert_eq!(got, [[total.clone(), total, mark(na), mark(nb)]], "{what}");
                let txn = db.begin(TxnOptions::read_only()).unwrap();
                assert_eq!(
                    nucleus_sql::check::check(&*txn, setup.registry()).unwrap(),
                    Vec::<String>::new(),
                    "{what}"
                );
            }
        }
    }
}

/// A statement that fails part-way through a column table's writes
/// leaves nothing behind for the next statement: the rows it gathered go
/// with it (they are kept per table until the statement ends, and a
/// failure before the table was noted as written once left them there —
/// the next statement on the same thread then stored them).
#[test]
fn a_failed_statement_leaves_no_gathered_rows() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(8)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(Arc::clone(&db));
    s.execute(
        "CREATE TABLE c (id INT PRIMARY KEY, u INT) WITH (storage = column); \
         INSERT INTO c SELECT i, i FROM generate_series(1, 5000) i; \
         CREATE UNIQUE INDEX c_u ON c (u)",
    )
    .unwrap();
    // The update's new row is gathered, then its index entry conflicts.
    let e = s.execute("UPDATE c SET u = 5 WHERE id = 6").unwrap_err();
    assert_eq!(e.code, "23505", "{e:?}");
    s.execute("INSERT INTO c VALUES (9999, 9999)").unwrap();
    let got = outcome(
        &mut s,
        "SELECT count(*), count(DISTINCT id), sum(CASE WHEN id = 6 THEN u END) FROM c",
    )
    .unwrap();
    assert_eq!(got, [["5001", "5001", "6"]]);
    let txn = db.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
        Vec::<String>::new()
    );
}

/// An enum's values in segments read with their labels, as the reading
/// transaction's catalog gives them (a bug found by W3b's load test: they
/// read without label or type): in rows, in batches, by key, after a
/// label is renamed with the segments' columns already cached.
#[test]
fn enum_values_in_segments_read_their_labels() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(11)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(db);
    s.execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'glad'); \
         CREATE TABLE r (id INT PRIMARY KEY, m mood, x INT); \
         CREATE TABLE c (id INT PRIMARY KEY, m mood, x INT) WITH (storage = column); \
         INSERT INTO r SELECT i, (ARRAY['sad', 'ok', 'glad'])[1 + i % 3]::mood, i * 7 FROM generate_series(1, 9000) i; \
         INSERT INTO c SELECT * FROM r; \
         CREATE INDEX ON r (x); CREATE INDEX ON c (x)",
    )
    .unwrap();
    let queries = [
        "SELECT min(m::text), max(m::text), count(*) FROM TT",
        "SELECT m, count(*) FROM TT GROUP BY m ORDER BY m",
        "SELECT id, m FROM TT WHERE id IN (3, 4, 5) ORDER BY id",
        "SELECT * FROM TT WHERE id = 8000",
        "SELECT m FROM TT WHERE x = 777", // by the index: a point read
        "SELECT count(*) FROM TT WHERE m = 'ok'",
        "SELECT m::text || '!' FROM TT ORDER BY id LIMIT 4",
    ];
    for q in queries {
        both(&mut s, q);
    }
    s.execute("ALTER TYPE mood RENAME VALUE 'ok' TO 'fine'")
        .unwrap();
    for q in queries {
        both(&mut s, &q.replace("'ok'", "'fine'"));
    }
    assert_eq!(
        outcome(&mut s, "SELECT m FROM c WHERE x = 7").unwrap(),
        vec![vec!["fine".to_string()]]
    );
}

/// A batch of rows in key order goes to a column table at once when the
/// table holds none of its keys (PLAN.md §M14.20 W3b) — so a batch that
/// meets a stored row must find it: rows gathered by the statement, rows
/// in the delta store, rows in segments; as a row table does.
#[test]
fn a_batch_meeting_stored_rows_finds_their_keys() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(12)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(db);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, v TEXT); \
         CREATE TABLE c (id INT PRIMARY KEY, v TEXT) WITH (storage = column)",
    )
    .unwrap();
    for sql in [
        // The second batch (rows 4097 on: 100 to 4195, in order) meets the
        // first, gathered.
        "INSERT INTO TT SELECT i, 'a' FROM generate_series(1, 4096) i \
         UNION ALL SELECT i, 'b' FROM generate_series(100, 4195) i",
        // A few rows, left in the delta store; then a batch over one.
        "INSERT INTO TT VALUES (50000, 'd'), (50010, 'd')",
        "INSERT INTO TT SELECT i, 'e' FROM generate_series(49000, 50005) i",
        "INSERT INTO TT SELECT i, 'e' FROM generate_series(50011, 60000) i",
        // Rows in segments (the load above); a batch past them, one over
        // their last.
        "INSERT INTO TT SELECT i, 'f' FROM generate_series(59990, 61000) i",
        "INSERT INTO TT SELECT i, 'f' FROM generate_series(60001, 61000) i",
        "INSERT INTO TT SELECT i, 'g' FROM generate_series(1, 10) i",
        "INSERT INTO TT SELECT i, 'g' FROM generate_series(0, 1) i",
        // A key twice in a row within a batch.
        "INSERT INTO TT VALUES (70000, 'h'), (70000, 'i')",
        "INSERT INTO TT SELECT 80000 + i / 2, 'j' FROM generate_series(0, 5000) i",
    ] {
        both(&mut s, sql);
        both(&mut s, "SELECT count(*), sum(id), min(v), max(v) FROM TT");
    }
}

#[test]
fn a_column_sample_takes_what_the_rows_give() {
    use nucleus_sql::ext::{Sample, StorageScan};
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(13)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(db.clone());
    s.execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
         CREATE TABLE c (id INT PRIMARY KEY, x INT, s TEXT, m mood) WITH (storage = column); \
         INSERT INTO c SELECT i, i % 97, 'name ' || (i % 13), \
             CASE i % 3 WHEN 0 THEN 'sad' WHEN 1 THEN 'ok' ELSE 'happy' END::mood \
             FROM generate_series(1, 100000) i; \
         DELETE FROM c WHERE id % 10 = 3; \
         UPDATE c SET x = -x WHERE id % 1000 = 7; \
         INSERT INTO c VALUES (0, 1, 'first', 'ok'), (100001, 2, 'last', NULL)",
    )
    .unwrap();
    let txn = db.begin(TxnOptions::read_only()).unwrap();
    let t = nucleus_sql::catalog::table(&*txn, "c").unwrap();
    let storage = nucleus_sql::storage::of(&t).unwrap();
    let scan = || {
        let start = t.index_prefix(0);
        let end = nucleus_kv::prefix_end(&start);
        StorageScan {
            txn: &*txn,
            table: &t,
            start,
            end,
            wanted: None,
            validated: false,
            bounds: Vec::new(),
            dicts: Vec::new(),
        }
    };
    // Its own sample (segments decoded only when a row of theirs is taken)
    // against the one every row of a scan makes: the same rows, the same
    // draws — sizes that take from every segment, from a few, and all.
    for size in [500, 3, 200_000] {
        let mut own = Sample::new(size, 11);
        storage.sample(scan(), &mut own).unwrap();
        let mut every = Sample::new(size, 11);
        for kr in storage.rows(scan()).unwrap() {
            let (_, row) = kr.unwrap();
            every.offer(|| Ok(row)).unwrap();
        }
        assert_eq!(own.seen(), 90_002, "{size}");
        assert_eq!(own.seen(), every.seen(), "{size}");
        assert_eq!(own.into_rows(), every.into_rows(), "{size}");
    }
}
