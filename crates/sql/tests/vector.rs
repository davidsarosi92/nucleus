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
