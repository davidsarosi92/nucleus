//! The vectorized executor (PLAN.md §M14.20 V2) against the row-at-a-time
//! one: every statement gives the same rows — or the same error — with
//! `batch_size` 0 (rows), 3 (batch boundaries everywhere) and the default.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session};

fn session(db: &Database, batch: usize) -> Session {
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(&format!("SET batch_size = {batch}")).unwrap();
    s
}

/// A statement's rows (rendered) or its error code and message.
fn outcome(s: &mut Session, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match s.execute(sql) {
        Ok(mut out) => match out.pop() {
            Some(Output::Rows { rows, .. }) => Ok(rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect()),
            other => panic!("{sql}: {other:?}"),
        },
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

/// Checks that `queries` agree across batch sizes; returns how many failed
/// (the same way in each).
fn agree(db: &Database, queries: &[String]) -> usize {
    let mut sessions: Vec<Session> = [0, 3, 1024].iter().map(|&b| session(db, b)).collect();
    let mut errors = 0;
    for q in queries {
        let want = outcome(&mut sessions[0], q);
        for (s, b) in sessions.iter_mut().zip([0, 3, 1024]).skip(1) {
            assert_eq!(outcome(s, q), want, "batch_size {b}: {q}");
        }
        errors += usize::from(want.is_err());
    }
    errors
}

fn database() -> Database {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(5)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 0);
    s.execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
         CREATE TABLE t (id INT PRIMARY KEY, a INT, b BIGINT, f FLOAT, r REAL, ok BOOL, \
           s TEXT, n NUMERIC, d DATE, m mood, big TEXT); \
         INSERT INTO t SELECT i, \
           CASE WHEN i % 11 = 0 THEN NULL ELSE i % 23 - 11 END, \
           i::bigint * 7919 % 1000003, \
           CASE WHEN i % 13 = 0 THEN NULL ELSE (i % 50) / 7.0 END, \
           (i % 9)::real / 4, \
           CASE WHEN i % 5 = 0 THEN NULL ELSE i % 3 = 0 END, \
           'name ' || (i % 37), (i % 101) / 3.0, DATE '2026-01-01' + i % 400, \
           (ARRAY['sad', 'ok', 'happy'])[1 + i % 3]::mood, \
           CASE WHEN i % 500 = 0 THEN repeat('x', 20000) END \
           FROM generate_series(1, 3000) i",
    )
    .unwrap();
    db
}

#[test]
fn chosen_queries_agree() {
    let db = database();
    let queries: Vec<String> = [
        "SELECT * FROM t WHERE a > 3 ORDER BY id",
        "SELECT id, a + b, a * 2 - 1, b / 7, b % 5, -a FROM t WHERE ok ORDER BY id",
        "SELECT id, f * a, f + 1, f / 2, a < f, f = 2 FROM t WHERE f > 3.5 ORDER BY id",
        "SELECT count(*), count(a), sum(a), sum(b), avg(a), avg(f), min(a), max(b), min(f), \
           max(s), sum(n), avg(n), min(d), max(m), sum(r), string_agg(s, ',') FROM t WHERE id < 200",
        "SELECT a, count(*), sum(b), avg(f), max(s) FROM t GROUP BY a ORDER BY a",
        "SELECT s, m, count(*), sum(a) FROM t GROUP BY s, m ORDER BY s, m",
        "SELECT ok, count(*) FROM t GROUP BY ok ORDER BY ok",
        "SELECT f, count(*) FROM t GROUP BY f ORDER BY f",
        "SELECT a % 3, sum(b) FROM t WHERE a IS NOT NULL GROUP BY a % 3 ORDER BY 1",
        "SELECT count(*) FROM t WHERE a IS NULL OR f IS NULL",
        "SELECT count(*) FROM t WHERE NOT ok AND a BETWEEN -2 AND 5",
        "SELECT id FROM t WHERE s LIKE 'name 1%' AND m = 'ok' ORDER BY id",
        "SELECT id, s FROM t WHERE s = 'name 3' OR s < 'name 10' ORDER BY id",
        "SELECT count(*) FROM t WHERE s LIKE '%7' OR s NOT LIKE '%e 1%'",
        "SELECT count(*) FROM t WHERE s LIKE '%' AND s LIKE 'name 2' AND s LIKE '%me%'",
        "SELECT count(*) FROM t WHERE s LIKE 'name _' OR s ILIKE 'NAME 3%'",
        "SELECT id FROM t WHERE upper(s) LIKE 'NAME 3%' AND upper(s) >= 'NAME 35' ORDER BY id",
        "SELECT count(*) FROM t WHERE s > big OR s = s",
        "SELECT id, d FROM t WHERE d = DATE '2026-02-01' OR d BETWEEN DATE '2026-12-30' AND \
           DATE '2027-01-02' ORDER BY id",
        "SELECT count(*) FROM t WHERE d > now()::date - 1000 AND d + 1 > DATE '2026-03-01'",
        "SELECT d, count(*) FROM t GROUP BY d ORDER BY d LIMIT 5",
        "SELECT s, count(*), min(d), max(d) FROM t GROUP BY s ORDER BY s",
        "SELECT id, length(big) FROM t WHERE big IS NOT NULL ORDER BY id",
        "SELECT id, upper(s), coalesce(a, -100), CASE WHEN a > 0 THEN 'p' ELSE 'n' END \
           FROM t WHERE id % 97 = 1 ORDER BY id",
        "SELECT id, d + 1, n * 2, r * 2 FROM t WHERE d > DATE '2026-12-01' ORDER BY id",
        "SELECT id, (SELECT count(*) FROM t u WHERE u.a = t.a) FROM t WHERE id < 20 ORDER BY id",
        "SELECT sum(a) FROM t WHERE id IN (SELECT id FROM t WHERE b % 2 = 0)",
        "SELECT id FROM t WHERE a = 1 LIMIT 5",
        "SELECT * FROM t WHERE id > 2990",
        "SELECT * FROM t WHERE id > 5000",
        "SELECT count(*), sum(a) FROM t WHERE id > 5000",
        "SELECT a, sum(b) FROM t WHERE id > 5000 GROUP BY a",
        // Errors: after the rows before them, and none past a LIMIT.
        "SELECT id, 100 / (id - 50) FROM t WHERE id > 0 ORDER BY id",
        "SELECT id, 100 / (id - 50) FROM t WHERE id > 0 LIMIT 10",
        "SELECT id FROM t WHERE id <> 50 AND 100 / (id - 50) > 1 ORDER BY id",
        "SELECT id FROM t WHERE id = 50 OR 100 / (id - 50) > 1 ORDER BY id",
        "SELECT sum(100 / (id - 50)) FROM t",
        "SELECT a, sum(100 / (id - 50)) FROM t GROUP BY a",
        "SELECT sum(b * 922337203685477) FROM t WHERE id < 3",
        "SELECT id FROM t WHERE s::int > 3",
        "SELECT sum(s) FROM t",
        "SELECT id FROM t WHERE a",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    assert_eq!(agree(&db, &queries), 7, "the error cases fail");
}

#[test]
fn integer_sums_overflow_only_when_the_result_does() {
    let db = database();
    let mut s = session(&db, 0);
    s.execute(
        "CREATE TABLE big (id INT PRIMARY KEY, x BIGINT); \
         INSERT INTO big VALUES (1, 9000000000000000000), (2, 9000000000000000000), \
           (3, -9000000000000000000)",
    )
    .unwrap();
    let q = [
        "SELECT sum(x), avg(x) FROM big".to_string(),
        "SELECT sum(x) FROM big WHERE id < 3".to_string(),
    ];
    assert_eq!(agree(&db, &q), 1);
    assert_eq!(
        outcome(&mut s, &q[0]),
        Ok(vec![vec![
            "9000000000000000000".into(),
            "3000000000000000000".into()
        ]])
    );
}

#[test]
fn groups_past_work_mem_agree() {
    let db = database();
    let mut sessions: Vec<Session> = [0, 3, 1024].iter().map(|&b| session(&db, b)).collect();
    let q = "SELECT b % 2000, count(*), sum(a), max(s) FROM t GROUP BY b % 2000";
    let mut want = None;
    for s in &mut sessions {
        s.execute("SET work_mem = '64kB'").unwrap();
        let mut got = outcome(s, q).unwrap();
        got.sort();
        assert_eq!(*want.get_or_insert_with(|| got.clone()), got);
    }
}

#[test]
fn many_groups_past_work_mem_spill_on_the_workers() {
    // Many groups in no order of their keys, past work_mem: the morsels
    // write their groups to scratch and the parts read them back on the
    // workers (no one thread partitioning) — the rows of one pass, and its
    // first error; a part at a time, each in the order first seen, so
    // whatever the morsels.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(31)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE g (id INT PRIMARY KEY, k BIGINT, v BIGINT, t TEXT) \
               WITH (storage = {storage}); \
             INSERT INTO g SELECT i, (i * 7919) % 20000, \
               CASE WHEN i % 9973 = 0 THEN 9223372036854775807 ELSE i END, \
               CASE WHEN i % 5 = 0 THEN NULL ELSE 'v' || (i % 3000) END \
               FROM generate_series(1, 60000) i; \
             ANALYZE"
        ))
        .unwrap();
        let queries = [
            "SELECT k, count(*), min(v), max(t) FROM g GROUP BY k",
            "SELECT k % 7000, t, count(*), sum(id) FROM g GROUP BY 1, 2",
            "SELECT k, avg(id), count(t) FROM g WHERE id % 3 <> 0 GROUP BY k",
            // Sums past a bigint: the first group a single pass finishes.
            "SELECT k, sum(v) FROM g GROUP BY k",
        ];
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        let mut small = session(&db, 1024);
        small
            .execute("SET parallel_workers = 4; SET work_mem = '64kB'")
            .unwrap();
        let mut two = session(&db, 1024);
        two.execute("SET parallel_workers = 2; SET work_mem = '64kB'")
            .unwrap();
        let sorted = |r: Result<Vec<Vec<String>>, String>| {
            r.map(|mut rows| {
                rows.sort();
                rows
            })
        };
        let before = nucleus_sql::vector::spilled_groupings();
        for q in queries {
            let want = outcome(&mut one, q);
            assert_eq!(outcome(&mut many, q), want, "{storage} workers: {q}");
            let spilled = outcome(&mut small, q);
            assert_eq!(outcome(&mut two, q), spilled, "{storage} order: {q}");
            assert_eq!(sorted(spilled), sorted(want), "{storage} work_mem: {q}");
        }
        assert!(
            outcome(&mut one, queries[3]).is_err(),
            "{storage}: an overflow"
        );
        // Few groups, each met by every morsel (in an order turning by one
        // each round): a morsel done past half of work_mem writes its
        // groups to scratch, and the rows fit in memory — so they come in
        // the order a single pass first sees them.
        let mut mid = session(&db, 1024);
        mid.execute("SET parallel_workers = 4; SET work_mem = '1MB'")
            .unwrap();
        let few = "SELECT (id % 1999 + id / 1999) % 1999, count(*) FROM g GROUP BY 1";
        let at = nucleus_sql::vector::spilled_groupings();
        assert_eq!(
            outcome(&mut mid, few),
            outcome(&mut one, few),
            "{storage}: {few}"
        );
        assert!(
            nucleus_sql::vector::spilled_groupings() > at,
            "{storage}: few groups spilled"
        );
        assert!(
            nucleus_sql::vector::spilled_groupings() - before >= queries.len() as u64,
            "{storage}: the groups went to scratch on the workers"
        );
    }
}

#[test]
fn a_limit_over_a_sort_keeps_its_rows_only() {
    // `ORDER BY … LIMIT n OFFSET m` keeps only the first n + m rows (a
    // heap, the rows built only when kept): rows m.. of the full sort —
    // ties in the input's order —, its first error; on one thread and on
    // workers, a row or a batch at a time, the heap past work_mem falling
    // back to the full sort.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(37)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE o (id INT PRIMARY KEY, k INT, t TEXT, x INT) \
               WITH (storage = {storage}); \
             INSERT INTO o SELECT i, CASE WHEN i % 17 = 0 THEN NULL ELSE (i * 7919) % 50 END, \
               't' || ((i * 31) % 997), CASE WHEN i = 23456 THEN 0 ELSE i % 13 + 1 END \
               FROM generate_series(1, 30000) i; \
             ANALYZE"
        ))
        .unwrap();
        // (A query, its rows with no LIMIT, and the slices taken.)
        let queries = [
            "SELECT id, k FROM o ORDER BY k",
            "SELECT id, k, t FROM o ORDER BY k DESC NULLS LAST, t",
            "SELECT * FROM o WHERE x > 3 ORDER BY t DESC",
            "SELECT k, count(*), min(id) FROM o GROUP BY k ORDER BY count(*) DESC",
            "SELECT t, count(*) AS c, sum(x) FROM o GROUP BY t ORDER BY c, t DESC",
            // An error in a key, at one row.
            "SELECT id FROM o ORDER BY 100 / x, id",
        ];
        let slices: [(usize, usize); 5] = [(0, 0), (10, 0), (7, 3), (100, 250), (6000, 20)];
        let mut sessions = Vec::new();
        for (batch, workers, mem) in [
            (0, 1, "64MB"),
            (3, 1, "64MB"),
            (1024, 1, "64MB"),
            (1024, 4, "64MB"),
            (1024, 4, "64kB"),
            (0, 1, "64kB"),
        ] {
            let mut s = session(&db, batch);
            s.execute(&format!(
                "SET parallel_workers = {workers}; SET work_mem = '{mem}'"
            ))
            .unwrap();
            sessions.push((
                format!("{storage} batch {batch} workers {workers} {mem}"),
                s,
            ));
        }
        let mut full = session(&db, 1024);
        full.execute("SET parallel_workers = 1").unwrap();
        for q in queries {
            let all = outcome(&mut full, q);
            for &(n, m) in &slices {
                let want = all
                    .clone()
                    .map(|rows| rows.into_iter().skip(m).take(n).collect::<Vec<_>>());
                let limited = format!("{q} LIMIT {n} OFFSET {m}");
                for (name, s) in &mut sessions {
                    assert_eq!(outcome(s, &limited), want, "{name}: {limited}");
                }
            }
        }
        assert!(
            outcome(&mut full, queries[5]).is_err(),
            "{storage}: a division by zero"
        );
    }
}

#[test]
fn random_queries_agree() {
    let db = database();
    let rng = SplitMix64::new(3);
    let cols = ["a", "b", "f", "id", "r", "n"];
    let ops = ["+", "-", "*", "/", "%"];
    let cmps = ["<", "<=", ">", ">=", "=", "<>"];
    let pick = |xs: &[&'static str]| xs[rng.below(xs.len() as u64) as usize];
    let term = || -> String {
        match rng.below(3) {
            0 => pick(&cols).to_string(),
            1 => format!("{}", rng.below(20) as i64 - 5),
            _ => format!(
                "({} {} {})",
                pick(&cols),
                pick(&ops),
                rng.below(9) as i64 - 2
            ),
        }
    };
    let pred = || -> String {
        let one = || format!("{} {} {}", term(), pick(&cmps), term());
        match rng.below(4) {
            0 => one(),
            1 => format!("{} AND {}", one(), one()),
            2 => format!("{} OR NOT ({})", one(), one()),
            _ => format!("{} IS NULL OR {}", pick(&cols), one()),
        }
    };
    let mut queries = Vec::new();
    for _ in 0..150 {
        queries.push(match rng.below(3) {
            0 => format!(
                "SELECT id, {}, {} FROM t WHERE {} ORDER BY id",
                term(),
                term(),
                pred()
            ),
            1 => format!(
                "SELECT count(*), sum({}), min({}), max({}), avg({}) FROM t WHERE {}",
                term(),
                term(),
                term(),
                pick(&cols),
                pred()
            ),
            _ => format!(
                "SELECT {} AS k, count(*), sum({}) FROM t WHERE {} GROUP BY 1 ORDER BY 1",
                term(),
                term(),
                pred()
            ),
        });
    }
    let failed = agree(&db, &queries);
    assert!(failed < queries.len() / 2, "{failed} of the queries failed");
}

#[test]
fn parallel_aggregates_agree() {
    // Big enough to split into morsels; the simulation offers one CPU, so
    // the workers are asked for.
    let db = Database::open_on(
        Arc::new(SimPlatform::new(8)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    s.execute(
        "CREATE TABLE p (id INT PRIMARY KEY, g INT, x BIGINT, f FLOAT, s TEXT, ok BOOL); \
         INSERT INTO p SELECT i, i % 37, i::bigint * 7919 % 100003 - 50000, \
           CASE WHEN i % 17 = 0 THEN NULL ELSE (i % 1000) * 0.1 + 1e-9 * i END, \
           'name ' || (i % 101), i % 3 = 0 FROM generate_series(1, 60000) i",
    )
    .unwrap();
    let queries = [
        "SELECT count(*), sum(x), avg(x), min(x), max(x), sum(f), avg(f), min(s), max(s) FROM p",
        "SELECT count(*), sum(x) FROM p WHERE x % 7 = 3 AND s LIKE 'name 1%'",
        "SELECT g, count(*), sum(x), avg(f), min(s), bool_and(ok), bool_or(ok) FROM p GROUP BY g",
        "SELECT s, count(*), sum(f), max(x) FROM p GROUP BY s",
        "SELECT ok, array_agg(id) FROM p WHERE id % 1000 = 7 GROUP BY ok",
        "SELECT g % 5, sum(x * 2), count(f) FROM p WHERE f > 50 GROUP BY 1",
        "SELECT count(*) FROM p WHERE id > 59990",
        "SELECT string_agg(s, ',') FROM p WHERE id < 5",
        "SELECT sum(100 / (id - 45000)) FROM p",
        "SELECT sum(x * 9223372036854775) FROM p",
        "SELECT * FROM p WHERE x % 7 = 3",
        "SELECT id, s FROM p WHERE s LIKE '%7' LIMIT 25",
        "SELECT id, 100 / (id - 45000) FROM p WHERE id > 0",
        "SELECT id, 100 / (id - 45000) FROM p WHERE id > 0 LIMIT 10",
        "SELECT id FROM p WHERE id BETWEEN 20000 AND 40000 AND ok",
        "SELECT g, count(*) FROM (SELECT * FROM p WHERE f < 10) q GROUP BY g ORDER BY g",
    ];
    let mut one = session(&db, 1024);
    one.execute("SET parallel_workers = 1").unwrap();
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    for q in queries {
        assert_eq!(outcome(&mut many, q), outcome(&mut one, q), "{q}");
    }
    assert!(
        nucleus_sql::vector::parallel_runs() - before >= 10,
        "the aggregates ran in parallel"
    );
}

#[test]
fn parallel_groups_in_key_order_past_work_mem_agree() {
    // Grouped by a prefix of the primary key, the morsels' groups meet only
    // at their edges: merged without a table of every group, so past
    // work_mem the parallel run still holds.
    let db = Database::open_on(
        Arc::new(SimPlatform::new(9)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    s.execute(
        "CREATE TABLE q (a INT, b INT, v BIGINT, t TEXT, PRIMARY KEY (a, b)); \
         INSERT INTO q SELECT i / 3, i % 3, i::bigint * 7919 % 100003, 'v' || (i % 7) \
         FROM generate_series(1, 60000) i",
    )
    .unwrap();
    let queries = [
        "SELECT a, count(*), sum(v), min(t) FROM q GROUP BY a ORDER BY a",
        "SELECT a, b % 2, sum(v) FROM q GROUP BY a, b % 2 ORDER BY 1, 2",
        "SELECT b, a, max(v) FROM q WHERE v % 5 <> 0 GROUP BY b, a ORDER BY 2, 1",
        "SELECT a, b, sum(v) FROM q GROUP BY a, b HAVING sum(v) > 99000 ORDER BY 1, 2",
        "SELECT count(*), sum(c) FROM (SELECT a, count(*) AS c FROM q GROUP BY a) x",
    ];
    let mut one = session(&db, 1024);
    one.execute("SET parallel_workers = 1; SET work_mem = '64kB'")
        .unwrap();
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4; SET work_mem = '64kB'")
        .unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    for q in queries {
        assert_eq!(outcome(&mut many, q), outcome(&mut one, q), "{q}");
    }
    assert!(
        nucleus_sql::vector::parallel_runs() - before >= 4,
        "the aggregates ran in parallel"
    );
}

#[test]
fn parallel_joins_and_sorts_agree() {
    // Hash joins probed by morsels of the larger side, sorts of morsels
    // merged: the rows, their order and errors those of one thread and of
    // the row-at-a-time executor.
    let db = Database::open_on(
        Arc::new(SimPlatform::new(10)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    s.execute(
        "CREATE TABLE f (id INT PRIMARY KEY, d INT, x BIGINT, s TEXT); \
         CREATE TABLE dim (id INT PRIMARY KEY, name TEXT, k INT); \
         INSERT INTO f SELECT i, i % 500, i::bigint * 7919 % 100003, 'v' || (i % 13) \
         FROM generate_series(1, 60000) i; \
         INSERT INTO dim SELECT i, 'd' || i, i % 7 FROM generate_series(0, 400) i",
    )
    .unwrap();
    let queries = [
        "SELECT f.id, dim.name FROM f JOIN dim ON f.d = dim.id WHERE f.x % 11 = 3",
        "SELECT count(*), sum(f.x) FROM f JOIN dim ON f.d = dim.id AND dim.k <> f.d % 7",
        "SELECT f.id, dim.name FROM f LEFT JOIN dim ON f.d = dim.id WHERE f.id % 97 = 1",
        "SELECT count(*) FROM f WHERE EXISTS (SELECT 1 FROM dim WHERE dim.id = f.d AND dim.k = 3)",
        "SELECT count(*) FROM f WHERE NOT EXISTS (SELECT 1 FROM dim WHERE dim.id = f.d)",
        "SELECT f.s, dim.name FROM f JOIN dim ON f.s = dim.name || '' WHERE f.id < 100",
        "SELECT f.id, 100 / (f.id - 30000) FROM f JOIN dim ON f.d = dim.id WHERE f.x > 50",
        "SELECT id, x FROM f WHERE x % 3 = 0 ORDER BY x DESC, id LIMIT 50",
        "SELECT s, x FROM f WHERE id % 5 = 0 ORDER BY s, x NULLS FIRST",
        "SELECT id FROM f WHERE x > 10 ORDER BY 100 / (id - 45000)",
        // Both sides large: the table made by workers too — each key's
        // rows in order, a key's error after the scan's.
        "SELECT count(*), sum(a.x), sum(b.x) FROM f a JOIN f b ON a.id = b.x",
        "SELECT a.id, b.id FROM f a JOIN f b ON a.id = b.d WHERE a.id < 30",
        "SELECT count(*) FROM f a JOIN f b ON a.id = b.x AND a.s = b.s",
        "SELECT count(*), count(b.id) FROM f a LEFT JOIN f b ON a.id = b.x * 2",
        "SELECT count(*) FROM f a JOIN f b ON a.id = 100 / (b.id - 30000)",
        "SELECT count(*) FROM f a JOIN f b ON 100 / (a.id - 30000) = b.x",
    ];
    let mut row = session(&db, 0);
    let mut one = session(&db, 1024);
    one.execute("SET parallel_workers = 1").unwrap();
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    // A table past work_mem: stashed, not made by workers — and a join
    // that spills partitions its rows, so their order is its own.
    let sorted = |r: Result<Vec<Vec<String>>, String>| {
        r.map(|mut rows| {
            rows.sort();
            rows
        })
    };
    let mut small = session(&db, 1024);
    small
        .execute("SET parallel_workers = 4; SET work_mem = '64kB'")
        .unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    for q in queries {
        let want = outcome(&mut row, q);
        assert_eq!(outcome(&mut one, q), want, "one thread: {q}");
        assert_eq!(outcome(&mut many, q), want, "workers: {q}");
        assert_eq!(
            sorted(outcome(&mut small, q)),
            sorted(want.clone()),
            "past work_mem: {q}"
        );
    }
    assert!(
        nucleus_sql::vector::parallel_runs() - before >= 8,
        "the joins and sorts ran in parallel"
    );
}

#[test]
fn parallel_writes_agree() {
    // `UPDATE` and `DELETE` find their rows through the vectorized scan, in
    // parallel on a large table: what they change, what they return and
    // their errors are those of one thread and of the row-at-a-time
    // executor — inside a transaction too, its own writes seen.
    fn database(seed: u64) -> Database {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        session(&db, 0)
            .execute(
                "CREATE TABLE w (id INT PRIMARY KEY, g INT, x BIGINT, s TEXT); \
                 CREATE INDEX w_x ON w (x); \
                 CREATE TABLE h (a INT, b TEXT); \
                 INSERT INTO w SELECT i, i % 37, i::bigint * 7919 % 100003, 'v' || (i % 13) \
                 FROM generate_series(1, 60000) i; \
                 INSERT INTO h SELECT i % 100, 'h' || i FROM generate_series(1, 20000) i",
            )
            .unwrap();
        db
    }
    let statements = [
        "DELETE FROM w WHERE id % 10 = 0",
        "UPDATE w SET x = x + 1 WHERE id % 4 = 0 AND s LIKE 'v1%'",
        "DELETE FROM w WHERE 100 / (id - 45001) > 1",
        "UPDATE w SET s = s || '!' WHERE 100 / (id - 30001) > 0",
        "DELETE FROM w WHERE g = 5 RETURNING id, x",
        "BEGIN; UPDATE w SET g = 99 WHERE id % 7 = 1; \
         DELETE FROM w WHERE g = 99 AND id % 2 = 0 RETURNING id; COMMIT",
        "BEGIN; INSERT INTO w SELECT i, 98, i, 'new' FROM generate_series(70000, 80000) i; \
         UPDATE w SET x = -x WHERE g = 98 AND id % 3 = 0 RETURNING id, x; COMMIT",
        "DELETE FROM h WHERE a % 3 = 0",
        "UPDATE h SET b = upper(b) WHERE a < 10",
        "SELECT count(*), sum(id), sum(x), min(s), max(s), sum(g) FROM w",
        "SELECT id, x FROM w WHERE x BETWEEN 1000 AND 1100 ORDER BY id",
        "SELECT a, count(*), min(b), max(b) FROM h GROUP BY a ORDER BY a",
    ];
    // Any output, rendered.
    fn run(s: &mut Session, sql: &str) -> String {
        match s.execute(sql) {
            Ok(out) => format!("{out:?}"),
            Err(e) => format!("{} {}", e.code, e.message),
        }
    }
    let (a, b, c) = (database(11), database(11), database(11));
    let mut row = session(&a, 0);
    let mut one = session(&b, 1024);
    one.execute("SET parallel_workers = 1").unwrap();
    let mut many = session(&c, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    let mut errors = 0;
    for q in statements {
        let want = run(&mut row, q);
        assert_eq!(run(&mut one, q), want, "one thread: {q}");
        assert_eq!(run(&mut many, q), want, "workers: {q}");
        errors += usize::from(want.starts_with("22012"));
    }
    assert_eq!(errors, 2, "both divisions by zero failed");
    assert!(
        nucleus_sql::vector::parallel_runs() - before >= 6,
        "the writes found their rows in parallel"
    );
}

/// `char(n)` columns as char vectors, against text and themselves; `IN`
/// lists, `substr` and `LIKE` over them; an uncorrelated subquery in a
/// filter — the same in row and column tables at every batch size.
#[test]
fn char_text_in_lists_and_substr_agree() {
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(12)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 0);
        s.execute(&format!(
            "CREATE TABLE c (id INT PRIMARY KEY, k char(6), j char(3), t TEXT, n INT) \
             WITH (storage = {storage}); \
             INSERT INTO c SELECT i, \
               CASE WHEN i % 17 = 0 THEN NULL WHEN i % 5 = 0 THEN 'é' || (i % 7) \
                 ELSE 'ab' || (i % 9) END, \
               CASE WHEN i % 4 = 0 THEN '' ELSE (i % 3)::text END, \
               CASE WHEN i % 13 = 0 THEN NULL WHEN i % 3 = 0 THEN 'ab' || (i % 9) || ' ' \
                 ELSE 'ab' || (i % 9) END, \
               CASE WHEN i % 11 = 0 THEN NULL ELSE i % 6 END \
             FROM generate_series(1, 2500) i"
        ))
        .unwrap();
        let queries: Vec<String> = [
            "SELECT id, k || '|' FROM c WHERE k = 'ab3' ORDER BY id",
            "SELECT count(*) FROM c WHERE k = 'ab3   '",
            "SELECT count(*) FROM c WHERE k <> 'ab3' AND k > 'ab1' AND k <= 'é4'",
            "SELECT count(*) FROM c WHERE k = 'ab3'::char(2)",
            "SELECT count(*) FROM c WHERE k = t",
            "SELECT count(*) FROM c WHERE t = k OR k < t",
            "SELECT count(*) FROM c WHERE k = j OR j = ''",
            "SELECT count(*) FROM c WHERE k IN ('ab1', 'ab2 ', 'é3')",
            "SELECT count(*) FROM c WHERE k NOT IN ('ab1', 'ab2')",
            "SELECT count(*) FROM c WHERE k IN ('ab1', NULL)",
            "SELECT count(*) FROM c WHERE k NOT IN ('ab1', NULL)",
            "SELECT count(*) FROM c WHERE t IN ('ab3', 'ab3 ', 'ab4')",
            "SELECT count(*) FROM c WHERE t IN ('ab3'::char(4), 'ab4')",
            "SELECT count(*) FROM c WHERE n IN (1, 3, 5)",
            "SELECT count(*) FROM c WHERE n NOT IN (1, 3, 5)",
            "SELECT count(*) FROM c WHERE n IN (1, 2.0)",
            "SELECT count(*) FROM c WHERE n IN (1, id % 4)",
            "SELECT count(*) FROM c WHERE id IN (n, n + 1, 7)",
            "SELECT substr(k, 1, 2), count(*) FROM c GROUP BY 1 ORDER BY 1",
            "SELECT id, substring(k from 2 for 1) || '|', substr(t, 3) || '|', substr(k, 0, 2), \
               substr(k, -1), substr(t, 2, 0), substr(k, 3, 100) || '|' FROM c \
               WHERE id % 7 = 1 ORDER BY id",
            "SELECT count(*) FROM c WHERE substring(k from 1 for 1) IN ('a', 'é')",
            "SELECT count(*) FROM c WHERE substr(k, 1, 2) IN ('ab') AND substr(t, 1, 2) = 'ab'",
            "SELECT id, substr(k, 1, n) FROM c WHERE id < 40 ORDER BY id",
            "SELECT id, substr(k, 1, -1) FROM c WHERE id < 40 ORDER BY id",
            "SELECT count(*) FROM c WHERE k LIKE 'ab%' OR k LIKE '%3' OR k LIKE 'ab3'",
            "SELECT count(*) FROM c WHERE k LIKE 'ab3   '",
            "SELECT count(*) FROM c WHERE n > (SELECT avg(n) FROM c WHERE k IN ('ab1', 'ab2'))",
            "SELECT count(*) FROM c WHERE n = (SELECT n FROM c WHERE id < 3)",
            "SELECT count(*) FROM c WHERE id < 0 AND n = (SELECT n FROM c WHERE id < 3)",
            "SELECT count(*) FROM c WHERE EXISTS (SELECT 1 FROM c WHERE k = 'zz') OR n = 1",
            "SELECT k, j, count(*) FROM c GROUP BY k, j ORDER BY k, j",
            "SELECT max(k) || '|', min(j) || '|' FROM c",
        ]
        .iter()
        .map(|q| q.to_string())
        .collect();
        let errors = agree(&db, &queries);
        assert_eq!(
            errors, 2,
            "{storage}: the subquery of rows and substr's negative count"
        );
    }
}

/// Index nested loops on worker threads give a single pass's rows, in its
/// order, and its errors: inner, left, semi and anti joins, by the primary
/// key and by a secondary index, on row and column tables.
#[test]
fn parallel_lookups_agree() {
    // (A column table's lookups cost more: fewer left rows look up.)
    for (storage, few) in [("row", 1500), ("column", 120)] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(13)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE big (id INT PRIMARY KEY, g INT, v INT, s char(4)) \
             WITH (storage = {storage}); \
             CREATE TABLE few (k INT PRIMARY KEY, ref INT, w INT); \
             INSERT INTO big SELECT i, i % 997, i * 7 % 1000, (i % 5)::text \
               FROM generate_series(1, 40000) i; \
             INSERT INTO few SELECT i, CASE WHEN i % 9 = 0 THEN NULL ELSE i * 37 % 45000 END, \
               i % 4 FROM generate_series(1, {few}) i; \
             CREATE INDEX big_g ON big (g); \
             ANALYZE"
        ))
        .unwrap();
        let queries = [
            "SELECT few.k, big.v, big.s FROM few JOIN big ON big.id = few.ref ORDER BY few.k",
            "SELECT few.k, big.v FROM few JOIN big ON big.id = few.ref",
            "SELECT few.k, big.v FROM few LEFT JOIN big ON big.id = few.ref AND big.v > 300",
            "SELECT k FROM few WHERE EXISTS (SELECT 1 FROM big WHERE big.id = few.ref AND v < 500)",
            "SELECT k FROM few WHERE NOT EXISTS (SELECT 1 FROM big WHERE big.id = few.ref)",
            "SELECT few.k, count(*) FROM few JOIN big ON big.g = few.k WHERE few.k < 60 \
               GROUP BY few.k ORDER BY few.k",
            "SELECT few.k, big.id FROM few JOIN big ON big.g = few.k AND big.s IN ('1', '3') \
               WHERE few.k < 40",
            "SELECT few.k, big.v / (big.v - 700) FROM few JOIN big ON big.id = few.ref",
            "SELECT few.k, big.v FROM few JOIN big ON big.id = few.ref LIMIT 7",
        ];
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        let looked_up = queries
            .iter()
            .filter(|q| {
                let plan = outcome(&mut many, &format!("EXPLAIN {q}")).unwrap();
                plan.iter().any(|l| l[0].contains("Index Nested Loop"))
            })
            .count();
        assert!(looked_up >= 3, "{storage}: {looked_up} index nested loops");
        let before = nucleus_sql::vector::parallel_runs();
        for q in queries {
            assert_eq!(
                outcome(&mut many, q),
                outcome(&mut one, q),
                "{storage}: {q}"
            );
        }
        assert!(
            nucleus_sql::vector::parallel_runs() - before >= looked_up as u64,
            "{storage}: the lookups ran on workers"
        );
    }
}

#[test]
fn columnar_join_tables_agree() {
    // A hash join's table made by workers holds its rows a column at a
    // time: every kind of value, NULLs, a column of only NULLs and
    // constants come back as a row-at-a-time join gives them.
    let db = Database::open_on(
        Arc::new(SimPlatform::new(14)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    s.execute(
        "CREATE TYPE tone AS ENUM ('low', 'mid', 'high'); \
         CREATE TABLE f (id INT PRIMARY KEY, k INT, b BIGINT, x FLOAT, r REAL, ok BOOL, \
           s TEXT, c char(3), n NUMERIC, d DATE, ts TIMESTAMP, t tone, bin BYTEA, z INT); \
         INSERT INTO f SELECT i, \
           CASE WHEN i % 7 = 0 THEN NULL ELSE i % 25000 END, \
           i::bigint * 7919 % 100003, \
           CASE WHEN i % 11 = 0 THEN NULL ELSE i / 8.0 END, (i % 9)::real / 4, \
           CASE WHEN i % 5 = 0 THEN NULL ELSE i % 3 = 0 END, \
           CASE WHEN i % 13 = 0 THEN NULL ELSE 'é' || (i % 41) END, \
           CASE WHEN i % 17 = 0 THEN NULL ELSE (i % 6)::text END, \
           (i % 101) / 3.0, DATE '2026-01-01' + i % 400, \
           TIMESTAMP '2026-01-01 00:00:00' + (i % 1000) * INTERVAL '1 minute', \
           (ARRAY['low', 'mid', 'high'])[1 + i % 3]::tone, \
           CASE WHEN i % 19 = 0 THEN NULL ELSE ('k' || (i % 255))::bytea END, \
           NULL \
         FROM generate_series(1, 50000) i",
    )
    .unwrap();
    let queries = [
        "SELECT a.id, b.* FROM f a JOIN f b ON a.id = b.k WHERE a.id % 997 = 1",
        "SELECT a.id, b.id, b.s, b.c, b.z FROM f a LEFT JOIN f b ON a.id = b.k * 3 \
           WHERE a.id % 499 = 2",
        "SELECT count(*), count(b.x), sum(b.n), min(b.ts), max(b.c) FROM f a \
           JOIN f b ON a.id = b.k",
        "SELECT a.id, b.id, b.t FROM f a JOIN f b ON a.s = b.s AND a.c = b.c \
           WHERE a.id < 4 AND b.id % 101 = 0",
        "SELECT a.id, b.id FROM f a JOIN f b ON a.c = b.c AND a.d = b.d WHERE a.id < 30",
        "SELECT a.id, b.one, b.k FROM f a JOIN (SELECT k, 1 AS one, 'x' AS s FROM f) b \
           ON a.id = b.k WHERE a.id % 1999 = 0",
        "SELECT count(*) FROM f a LEFT JOIN f b ON a.id = b.z",
    ];
    let mut row = session(&db, 0);
    let mut one = session(&db, 1024);
    one.execute("SET parallel_workers = 1").unwrap();
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    for q in queries {
        let want = outcome(&mut row, q);
        assert!(want.is_ok(), "{q}: {want:?}");
        assert_eq!(outcome(&mut one, q), want, "one thread: {q}");
        assert_eq!(outcome(&mut many, q), want, "workers: {q}");
    }
    assert!(
        nucleus_sql::vector::parallel_runs() - before >= 5,
        "the tables were made by workers"
    );
}

#[test]
fn like_patterns_and_in_subqueries_agree() {
    // `%` inside a pattern (a kernel of its own), `IN (subquery)` as a
    // list — hashed when long, NULLs in it, text against char(n) — on
    // row and column tables; and the subqueries run once, before workers
    // look up or scan with them.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(15)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE w (id INT PRIMARY KEY, g INT, s TEXT, c char(6)) \
               WITH (storage = {storage}); \
             CREATE TABLE few (k INT PRIMARY KEY, ref INT, s TEXT); \
             INSERT INTO w SELECT i, CASE WHEN i % 10 = 0 THEN NULL ELSE i % 300 END, \
               CASE WHEN i % 23 = 0 THEN NULL \
                 ELSE (ARRAY['ab', 'xaybz', 'ñaña', 'a%b', 'zzab', ''])[1 + i % 6] || (i % 7) END, \
               CASE WHEN i % 29 = 0 THEN NULL ELSE 'c' || (i % 9) END \
             FROM generate_series(1, 30000) i; \
             INSERT INTO few SELECT i, i * 37 % 31000, 'c' || (i % 12) \
               FROM generate_series(1, 400) i; \
             ANALYZE"
        ))
        .unwrap();
        let queries: Vec<String> = [
            "SELECT id, s FROM w WHERE s LIKE '%a%b%' ORDER BY id LIMIT 40",
            "SELECT count(*) FROM w WHERE s NOT LIKE '%a%b%'",
            "SELECT count(*) FROM w WHERE s LIKE 'x%y%z%'",
            "SELECT count(*) FROM w WHERE s LIKE 'ñ%a%3'",
            "SELECT count(*) FROM w WHERE s LIKE 'a%%b%' OR s LIKE '%%'",
            "SELECT count(*) FROM w WHERE c LIKE 'c%3%'",
            "SELECT count(*) FROM w WHERE c LIKE '%c%'",
            "SELECT count(*) FROM w WHERE g IN (SELECT k FROM few WHERE k < 5)",
            "SELECT count(*) FROM w WHERE g IN (SELECT k * 2 FROM few)",
            "SELECT count(*) FROM w WHERE g NOT IN (SELECT k FROM few)",
            "SELECT count(*) FROM w WHERE g NOT IN (SELECT CASE WHEN k = 3 THEN NULL ELSE k END \
               FROM few)",
            "SELECT count(*) FROM w WHERE c IN (SELECT s FROM few)",
            "SELECT count(*) FROM w WHERE s IN (SELECT 'ab' || k FROM few WHERE k < 20)",
            "SELECT count(*) FROM w WHERE g IN (SELECT 100 / (k - 7) FROM few)",
            "SELECT few.k, w.s FROM few JOIN w ON w.id = few.ref \
               AND w.g NOT IN (SELECT k FROM few WHERE k % 3 = 0) ORDER BY few.k",
        ]
        .iter()
        .map(|q| q.to_string())
        .collect();
        assert_eq!(agree(&db, &queries), 1, "{storage}: one division by zero");
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        for q in &queries {
            assert_eq!(
                outcome(&mut many, q),
                outcome(&mut one, q),
                "{storage}: {q}"
            );
        }
    }
}

#[test]
fn key_filters_agree() {
    // A hash join's keys narrow the scans below its probe side: the rows
    // and errors of every executor stay the same — a row at a time, by
    // batches, on workers, past work_mem — and EXPLAIN ANALYZE says so.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(17)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE fact (id INT PRIMARY KEY, d INT, e INT, v INT, s TEXT) \
               WITH (storage = {storage}); \
             CREATE TABLE dim (id INT PRIMARY KEY, k INT, name TEXT); \
             CREATE TABLE p (id INT, g INT) PARTITION BY RANGE (id); \
             CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (20000); \
             CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (20000) TO (90000); \
             INSERT INTO fact SELECT i, CASE WHEN i % 13 = 0 THEN NULL ELSE i % 5000 END, \
               i % 97, i % 1000, 'v' || (i % 7) FROM generate_series(1, 40000) i; \
             INSERT INTO dim SELECT i, i % 10, 'd' || i FROM generate_series(0, 4999) i; \
             INSERT INTO p SELECT i, i % 300 FROM generate_series(0, 50000) i; \
             ANALYZE"
        ))
        .unwrap();
        let queries: Vec<String> = [
            // A few keys (a list), many (a Bloom filter), none.
            "SELECT f.id, dim.name FROM fact f JOIN dim ON f.d = dim.id WHERE dim.k = 3 \
               AND dim.id < 200",
            "SELECT count(*), sum(f.v) FROM fact f JOIN dim ON f.d = dim.id WHERE dim.k <> 3",
            "SELECT count(*) FROM fact f JOIN dim ON f.d = dim.id WHERE dim.k > 100",
            // An error on a row that joins, and one on rows the key filter
            // drops (no error then): the same in every executor.
            "SELECT f.id, 100 / (f.v - 503) FROM fact f JOIN dim ON f.d = dim.id \
               WHERE dim.k = 3 AND dim.id < 1000",
            "SELECT count(*) FROM fact f JOIN dim ON f.d = dim.id WHERE dim.k = 1 \
               AND 100 / (f.v - 500) > 0",
            // Through another join, a projection, a sort; two keys; a semi join.
            "SELECT f.id, a.name, b.name FROM fact f JOIN dim a ON f.d = a.id \
               JOIN dim b ON f.e = b.id WHERE a.k = 2 AND b.k = 7 ORDER BY f.id",
            "SELECT x.id FROM (SELECT id, d FROM fact ORDER BY v) x JOIN dim ON x.d = dim.id \
               WHERE dim.k = 4 ORDER BY x.id LIMIT 20",
            "SELECT count(*) FROM fact f JOIN dim ON f.d = dim.id AND f.e = dim.k \
               WHERE dim.name LIKE 'd1%'",
            "SELECT count(*) FROM fact f WHERE f.d IN (SELECT id FROM dim WHERE k = 5)",
            // Not narrowed: a left join, an anti join, a whole table.
            "SELECT count(*), count(dim.id) FROM fact f LEFT JOIN dim ON f.d = dim.id \
               AND dim.k = 3",
            "SELECT count(*) FROM fact f WHERE NOT EXISTS (SELECT 1 FROM dim WHERE dim.id = f.d \
               AND dim.k = 3)",
            "SELECT count(*) FROM fact f JOIN dim ON f.d = dim.id",
            // Partitions, and a correlated subquery that joins once per row.
            "SELECT count(*) FROM p JOIN dim ON p.g = dim.id WHERE dim.k = 6",
            "SELECT dim.id, (SELECT count(*) FROM fact f JOIN dim d2 ON f.d = d2.id \
               WHERE d2.k = dim.k AND d2.id < 300) FROM dim WHERE dim.id < 12",
        ]
        .iter()
        .map(|q| q.to_string())
        .collect();
        assert_eq!(agree(&db, &queries), 1, "{storage}: one division by zero");
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        let mut small = session(&db, 1024);
        small
            .execute("SET parallel_workers = 4; SET work_mem = '64kB'")
            .unwrap();
        let sorted = |r: Result<Vec<Vec<String>>, String>| {
            r.map(|mut rows| {
                rows.sort();
                rows
            })
        };
        for q in &queries {
            let want = outcome(&mut one, q);
            assert_eq!(outcome(&mut many, q), want, "{storage} workers: {q}");
            assert_eq!(
                sorted(outcome(&mut small, q)),
                sorted(want),
                "{storage} work_mem: {q}"
            );
        }
        let plan = outcome(&mut many, &format!("EXPLAIN ANALYZE {}", queries[0])).unwrap();
        assert!(
            plan.iter()
                .any(|l| l[0].contains("Scan on fact") && l[0].contains("join key filter")),
            "{storage}: {plan:?}"
        );
        let plan = outcome(&mut many, &format!("EXPLAIN ANALYZE {}", queries[9])).unwrap();
        assert!(
            !plan.iter().any(|l| l[0].contains("join key filter")),
            "{storage}: a left join filters nothing: {plan:?}"
        );
    }
}

#[test]
fn eager_aggregates_and_parallel_merges_agree() {
    // A grouping by a join's left key aggregated on the right side first
    // (eager aggregation), and many groups merged on the workers — in key
    // order or not: the rows, their order and the errors of a single pass.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(23)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE c (id INT PRIMARY KEY, name TEXT, k INT); \
             CREATE TABLE u (id INT UNIQUE, name TEXT); \
             CREATE TABLE o (id INT PRIMARY KEY, cid INT, amt BIGINT, f FLOAT, n NUMERIC, \
               s TEXT, ok BOOL, big BIGINT) WITH (storage = {storage}); \
             CREATE TABLE q (a INT, b INT, v BIGINT, big BIGINT, PRIMARY KEY (a, b)) \
               WITH (storage = {storage}); \
             INSERT INTO c SELECT i, 'c' || i, i % 10 FROM generate_series(1, 3000) i; \
             INSERT INTO u SELECT i, 'u' || i FROM generate_series(1, 3000) i; \
             INSERT INTO u VALUES (NULL, 'none'), (NULL, 'none again'); \
             INSERT INTO o SELECT i, CASE WHEN i % 17 = 0 THEN NULL ELSE i % 2400 END, \
               i::bigint * 7919 % 100003, CASE WHEN i % 13 = 0 THEN NULL ELSE i * 0.25 END, \
               (i % 101) / 3.0, 's' || (i % 41), i % 3 = 0, \
               CASE WHEN i % 50 = 0 THEN 5000000000000000000 ELSE i END \
               FROM generate_series(1, 60000) i; \
             INSERT INTO q SELECT i / 3, i % 3, i::bigint * 7919 % 100003, \
               CASE WHEN i / 3 % 7000 = 999 THEN 5000000000000000000 ELSE i END \
               FROM generate_series(1, 60000) i; \
             ANALYZE"
        ))
        .unwrap();
        let eager = [
            "SELECT c.id, count(o.id) FROM c LEFT JOIN o ON c.id = o.cid GROUP BY c.id",
            "SELECT c.id, c.name, count(*), count(o.f), min(o.s), max(o.amt), bool_and(o.ok), \
               bool_or(o.ok) FROM c LEFT JOIN o ON c.id = o.cid GROUP BY c.id, c.name \
               ORDER BY c.id",
            "SELECT c.id, count(*), min(o.f) FROM c JOIN o ON o.cid = c.id \
               GROUP BY c.id ORDER BY 2 DESC, 1",
            "SELECT n, count(*) FROM (SELECT c.id, count(o.id) AS n FROM c LEFT JOIN o \
               ON c.id = o.cid AND o.s NOT LIKE 's1%' GROUP BY c.id) x GROUP BY n ORDER BY 1",
        ];
        let not_eager = [
            // Not the left side's key; an aggregate that can fail; a unique
            // index's NULLs (a group of their own); more than the keys; a
            // join that keeps few of the rows (its key filter narrows them).
            "SELECT c.id, count(*), min(o.f) FROM c JOIN o ON o.cid = c.id WHERE c.k = 3 \
               GROUP BY c.id ORDER BY 2 DESC, 1",
            "SELECT c.k, count(o.id) FROM c LEFT JOIN o ON c.id = o.cid GROUP BY c.k ORDER BY 1",
            "SELECT c.id, sum(o.amt) FROM c LEFT JOIN o ON c.id = o.cid GROUP BY c.id",
            "SELECT u.id, count(o.id) FROM u LEFT JOIN o ON u.id = o.cid GROUP BY u.id",
            "SELECT c.id, count(o.id) FROM c LEFT JOIN o ON c.id = o.cid AND o.amt > c.k * 1000 \
               GROUP BY c.id",
        ];
        let merges = [
            // Many groups in no key order (merged a hash part a worker),
            // with and without an order.
            "SELECT amt % 30011, count(*), count(f), sum(amt), sum(n), sum(f), min(s), max(f), \
               bool_or(ok) FROM o GROUP BY 1",
            "SELECT cid, s, count(*), max(amt) FROM o GROUP BY cid, s ORDER BY 3 DESC, 1, 2 \
               LIMIT 50",
            // A bigint sum past its range: the first group a single pass
            // finishes fails.
            "SELECT id % 20000, sum(big) FROM o GROUP BY 1",
            // In key order: the morsels' edge groups merged, the others
            // finished on the workers.
            "SELECT a, count(*), sum(v), min(v) FROM q GROUP BY a",
            "SELECT a, b, sum(v) FROM q GROUP BY a, b HAVING sum(v) > 99000",
            "SELECT a, sum(big) FROM q GROUP BY a",
            "SELECT id, count(*), sum(n) FROM o GROUP BY id",
        ];
        let queries: Vec<String> = eager
            .iter()
            .chain(&not_eager)
            .chain(&merges)
            .map(|q| q.to_string())
            .collect();
        assert_eq!(agree(&db, &queries), 2, "{storage}: two bigint overflows");
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        let mut small = session(&db, 1024);
        small
            .execute("SET parallel_workers = 4; SET work_mem = '64kB'")
            .unwrap();
        // (Past work_mem, partitioned: the rows of a query with no order
        // in another.)
        let sorted = |r: Result<Vec<Vec<String>>, String>| {
            r.map(|mut rows| {
                rows.sort();
                rows
            })
        };
        let before = nucleus_sql::vector::parallel_runs();
        for q in &queries {
            let want = outcome(&mut one, q);
            assert_eq!(outcome(&mut many, q), want, "{storage} workers: {q}");
            assert_eq!(
                sorted(outcome(&mut small, q)),
                sorted(want),
                "{storage} work_mem: {q}"
            );
        }
        assert!(
            nucleus_sql::vector::parallel_runs() - before >= 10,
            "{storage}: the aggregates ran in parallel"
        );
        // Rewritten: the join over an aggregate (not only an aggregate over
        // the join).
        let mut aggregated_below = |q: &str| {
            let plan = outcome(&mut many, &format!("EXPLAIN {q}")).unwrap();
            let join = plan.iter().position(|l| l[0].contains("Join")).unwrap();
            plan[join..].iter().any(|l| l[0].contains("Aggregate"))
        };
        for q in eager {
            assert!(aggregated_below(q), "{storage} eager: {q}");
        }
        for q in not_eager {
            assert!(!aggregated_below(q), "{storage} not eager: {q}");
        }
    }
}

#[test]
fn join_and_group_tables_agree() {
    // The tables a join and a grouping keep: two integer keys hashed as a
    // pair, a join's output gathered as a build side in columns (spilled
    // when it outgrows work_mem), groups found by a salted hash of the
    // batch's own vectors or by a direct index over a narrow integer
    // range — the rows and errors of the row-at-a-time executor.
    for storage in ["row", "column"] {
        let db = Database::open_on(
            Arc::new(SimPlatform::new(29)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap();
        let mut s = session(&db, 1024);
        s.execute(&format!(
            "CREATE TABLE a (id INT PRIMARY KEY, x INT, y INT, t TEXT, c CHAR(6), v BIGINT) \
               WITH (storage = {storage}); \
             CREATE TABLE b (id INT PRIMARY KEY, x INT, y INT, w INT); \
             INSERT INTO a SELECT i, CASE WHEN i % 19 = 0 THEN NULL ELSE i % 300 END, i % 7, \
               't' || (i % 53), CASE WHEN i % 4 = 0 THEN 'ab' ELSE 'ab' || (i % 3) END, \
               CASE WHEN i % 1000 = 0 THEN -9223372036854775807 + i ELSE i % 400 - 200 END \
               FROM generate_series(1, 30000) i; \
             INSERT INTO b SELECT i, i % 350, i % 5, i % 11 FROM generate_series(1, 4000) i; \
             INSERT INTO b VALUES (5001, NULL, 1, 1), (5002, 3, NULL, 2); \
             ANALYZE"
        ))
        .unwrap();
        let queries: Vec<String> = [
            // Two integer keys: alone (the pair decides), with more to check.
            "SELECT count(*), sum(b.w) FROM a JOIN b ON a.x = b.x AND a.y = b.y",
            "SELECT count(*), sum(b.w) FROM a JOIN b ON a.x = b.x AND a.y = b.y AND b.w > a.v",
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.x = b.x AND a.y = b.y WHERE a.id < 60 \
               ORDER BY 1, 2",
            "SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.x = a.x AND b.y = a.y)",
            // A join's output as a build side; an error in its rows, and in
            // a key computed from them.
            "SELECT count(*), sum(j.w) FROM a JOIN (SELECT b.x, b.y, b.w FROM b JOIN b b2 \
               ON b.id = b2.id + 1) j ON a.x = j.x AND a.y = j.y",
            "SELECT count(*), sum(j.q) FROM a JOIN (SELECT b.x, 10 / (b.w - 4) AS q FROM b \
               JOIN b b2 ON b.id = b2.id) j ON a.x = j.x",
            "SELECT count(*) FROM a JOIN (SELECT b.x, b.w FROM b JOIN b b2 ON b.id = b2.id) j \
               ON a.x = j.x + 100 / (j.w - 6)",
            // Groups: text, char (its blanks), several keys, a narrow integer
            // range that widens to the extremes.
            "SELECT t, count(*), min(v) FROM a GROUP BY t",
            "SELECT c, count(*) FROM a GROUP BY c",
            "SELECT x, t, c, count(*), max(v) FROM a GROUP BY x, t, c",
            "SELECT y, count(*) FROM a GROUP BY y",
            "SELECT v, count(*) FROM a GROUP BY v",
            "SELECT v % 1000, count(*) FROM a GROUP BY 1 HAVING count(*) > 70",
            "SELECT x, count(*) FROM a GROUP BY x",
        ]
        .iter()
        .map(|q| q.to_string())
        .collect();
        assert_eq!(agree(&db, &queries), 2, "{storage}: two divisions by zero");
        let mut one = session(&db, 1024);
        one.execute("SET parallel_workers = 1").unwrap();
        let mut many = session(&db, 1024);
        many.execute("SET parallel_workers = 4").unwrap();
        let mut small = session(&db, 1024);
        small
            .execute("SET parallel_workers = 4; SET work_mem = '64kB'")
            .unwrap();
        let sorted = |r: Result<Vec<Vec<String>>, String>| {
            r.map(|mut rows| {
                rows.sort();
                rows
            })
        };
        for q in &queries {
            let want = outcome(&mut one, q);
            assert_eq!(outcome(&mut many, q), want, "{storage} workers: {q}");
            assert_eq!(
                sorted(outcome(&mut small, q)),
                sorted(want),
                "{storage} work_mem: {q}"
            );
        }
    }
}

#[test]
fn dictionary_filters_agree() {
    // A filter's leading tests of a text column against constants, on a
    // column table's dictionary segments, tested on the dictionary's
    // entries: the rows and errors of the same filter on a row table —
    // segments and delta rows, NULLs, char(n), and a conjunct after them
    // that fails on rows they drop (no error) or keep (the error).
    let db = Database::open_on(
        Arc::new(SimPlatform::new(31)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    for (t, storage) in [("r", "row"), ("c", "column")] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, mode TEXT, kind CHAR(8), v INT) \
               WITH (storage = {storage}); \
             INSERT INTO {t} SELECT i, \
               CASE WHEN i % 23 = 0 THEN NULL ELSE (ARRAY['AIR', 'MAIL', 'SHIP', 'TRUCK', \
                 'RAIL', 'FOB', 'REG AIR'])[1 + i % 7] END, \
               CASE WHEN i % 5 = 0 THEN 'big' ELSE 'small' || (i % 3) END, i % 1000 \
               FROM generate_series(1, 70000) i; \
             UPDATE {t} SET mode = 'MAIL!' WHERE id % 1000 = 1; \
             DELETE FROM {t} WHERE id % 997 = 0; \
             INSERT INTO {t} SELECT i, 'late', 'late', 7 FROM generate_series(70001, 70500) i"
        ))
        .unwrap();
    }
    let segments = outcome(
        &mut s,
        "SELECT count(*) FROM nc_catalog.segments WHERE table_name = 'c'",
    )
    .unwrap();
    assert!(segments[0][0].parse::<i64>().unwrap() >= 2, "{segments:?}");
    let filters = [
        "mode = 'MAIL'",
        "mode <> 'MAIL'",
        "mode IN ('AIR', 'REG AIR', 'late')",
        "mode NOT IN ('AIR', 'SHIP')",
        "mode LIKE '%AI%'",
        "mode NOT LIKE 'R%'",
        "mode ILIKE 'mail%'",
        "mode IS NULL",
        "mode IS NOT NULL AND kind = 'big'",
        "NOT (mode = 'SHIP' OR mode = 'FOB')",
        "mode > 'M' AND mode <= 'SHIP'",
        "mode = NULL",
        "kind = 'big'",
        "kind IN ('small1', 'late')",
        "kind LIKE 'small%' AND mode = 'TRUCK'",
        // A conjunct after them that fails: on rows they drop (no error),
        // on rows they keep (the error).
        // (`v = 1`'s rows are all 'MAIL!'.)
        "mode = 'MAIL' AND 100 / (v - 1) > 0",
        "mode = 'TRUCK' AND 100 / (v - 3) > 0",
        "mode IS NULL AND 100 / (v - 46) > 0",
        // First, it decides alone.
        "100 / (v - 3) > 0 AND mode = 'MAIL'",
        "mode = 'AIR' AND v > 500",
    ];
    let mut queries = Vec::new();
    for f in filters {
        for t in ["r", "c"] {
            queries.push(format!(
                "SELECT count(*), min(id), max(id) FROM {t} WHERE {f}"
            ));
            queries.push(format!(
                "SELECT id, mode, kind FROM {t} WHERE {f} AND id % 101 = 0"
            ));
        }
    }
    let before = nucleus_sql::vector::dictionary_tests();
    let errors = agree(&db, &queries);
    assert_eq!(
        errors,
        3 * 2 * 2,
        "three failing filters, two tables, two queries"
    );
    assert!(
        nucleus_sql::vector::dictionary_tests() - before >= 20,
        "the dictionaries' entries were tested"
    );
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    // (A filter's queries: on r, count and rows; then on c.)
    for four in queries.chunks(4) {
        for (r, c) in [(&four[0], &four[2]), (&four[1], &four[3])] {
            let want = outcome(&mut s, r);
            assert_eq!(outcome(&mut s, c), want, "{c}");
            assert_eq!(outcome(&mut many, c), want, "workers: {c}");
        }
    }
}

#[test]
fn decimal_kernels_agree() {
    // Decimal arithmetic and comparisons on one scale's mantissas, and sums
    // and averages straight from them: the rows and errors of the same
    // statements a row at a time — column segments and delta rows, row
    // tables, NULLs, integers among them, mixed scales, overflows (past 38
    // digits; a product's scale past 38) on rows in play or not, and
    // comparisons too far apart in scale to align.
    let db = Database::open_on(
        Arc::new(SimPlatform::new(37)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = session(&db, 1024);
    for (t, storage) in [("r", "row"), ("c", "column")] {
        s.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, g INT, k INT, p NUMERIC(12, 2), \
               d NUMERIC(4, 2), q NUMERIC(10, 0), h NUMERIC(38, 0), u NUMERIC) \
               WITH (storage = {storage}); \
             INSERT INTO {t} SELECT i, i % 7, CASE WHEN i % 9 = 0 THEN NULL ELSE i % 100 END, \
               CASE WHEN i % 17 = 0 THEN NULL ELSE (i % 100000) / 100.0 END::numeric(12, 2), \
               ((i % 11) / 100.0)::numeric(4, 2), (i % 1000)::numeric(10, 0), \
               10000000000000000000000000000000000000::numeric(38, 0) + i, \
               CASE WHEN i % 2 = 0 THEN 1.5 ELSE 2.25 END \
               FROM generate_series(1, 70000) i; \
             UPDATE {t} SET p = 0.01 WHERE id % 1000 = 1; \
             DELETE FROM {t} WHERE id % 997 = 0; \
             INSERT INTO {t} SELECT i, 1, 2, 3.5, 0.5, 7, 9, 0.125 FROM generate_series(70001, 70300) i"
        ))
        .unwrap();
    }
    let segments = outcome(
        &mut s,
        "SELECT count(*) FROM nc_catalog.segments WHERE table_name = 'c'",
    )
    .unwrap();
    assert!(segments[0][0].parse::<i64>().unwrap() >= 2, "{segments:?}");
    let big = "10000000000000000000000000000000";
    let selects = [
        "sum(p), avg(p), min(p), max(p), count(p)".to_string(),
        "sum(p * (1 - d)), sum(p * (1 - d) * (1 + d)), avg(d)".to_string(),
        "sum(p + id), sum(id * d), sum(p - k), sum(k * p), sum(q - p)".to_string(),
        "sum(u), sum(u * d), sum(u + p)".to_string(),
        "max(h + h), min(h - p), sum(h - h)".to_string(),
        // Errors: past 38 digits, on every row or some.
        "sum(h)".to_string(),
        "max(h * 10)".to_string(),
        format!("sum(p * {big})"),
        "sum(d * d * d * d * d * d * d * d * d * d * d * d * d * d * d * d * d * d * d * d)"
            .to_string(),
    ];
    let filters = [
        "true",
        "d BETWEEN 0.05 AND 0.07",
        "p > 500 AND p <= 600.5",
        "d = 0.04 OR p < 1",
        "p - d > 999",
        "p >= q",
        "p * 2 = q",
        "k < p",
        "h > 5",
        "h > 0.000001",
        "u = 1.50",
        "p IS NULL",
        // An overflow on rows the filter drops (none), and on rows it keeps.
        &format!("id < 5 AND p * {big} > 0"),
        &format!("id > 1500 AND p * {big} > 0"),
    ];
    let mut queries = Vec::new();
    for f in filters {
        for sel in &selects {
            for t in ["r", "c"] {
                queries.push(format!("SELECT {sel} FROM {t} WHERE {f}"));
            }
        }
        for t in ["r", "c"] {
            queries.push(format!(
                "SELECT g, sum(p * (1 - d)), avg(p), count(*) FROM {t} WHERE {f} \
                 GROUP BY g ORDER BY g"
            ));
        }
        for t in ["r", "c"] {
            queries.push(format!(
                "SELECT id, p * (1 - d), p + d, d - 1 FROM {t} WHERE {f} AND id % 701 = 0 \
                 ORDER BY id"
            ));
        }
    }
    let errors = agree(&db, &queries);
    assert!(errors > 0, "the overflows were met");
    let mut many = session(&db, 1024);
    many.execute("SET parallel_workers = 4").unwrap();
    // The same statement on r and on c.
    for pair in queries.chunks(2) {
        let want = outcome(&mut s, &pair[0]);
        assert_eq!(outcome(&mut s, &pair[1]), want, "{}", pair[1]);
        assert_eq!(outcome(&mut many, &pair[1]), want, "workers: {}", pair[1]);
    }
}
