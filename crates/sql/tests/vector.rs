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
