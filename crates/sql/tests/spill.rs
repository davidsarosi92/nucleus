//! Work past `work_mem` (PLAN.md §M14.11 L6): the same statements give the
//! same results with 64 kB as with the default, while the small limit
//! sends their sorts and materialised rows to scratch space — sealed,
//! when the database is encrypted (PLAN.md §M14.30 K2d).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, PageCipher};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn database() -> Database {
    open(false)
}

fn open(encrypted: bool) -> Database {
    Database::open_on(
        Arc::new(SimPlatform::new(11)),
        Path::new("/db"),
        Options {
            encryption_key: encrypted.then(|| PageCipher::new([1; 32])),
            ..Options::default()
        },
    )
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

/// Each query's rows, with `work_mem` as set by `set`.
fn results(db: &Database, set: &str, queries: &[&str]) -> Vec<Vec<Vec<Value>>> {
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(set).unwrap();
    queries.iter().map(|q| rows(&mut s, q)).collect()
}

#[test]
fn sorts_and_materialised_rows_spill_and_agree() {
    spill_and_agree(false);
}

#[test]
fn sealed_scratch_files_give_the_same_results() {
    spill_and_agree(true);
}

fn spill_and_agree(encrypted: bool) {
    let db = open(encrypted);
    {
        let kv: Arc<dyn Kv> = Arc::new(db.clone());
        let mut s = Session::new(kv);
        s.execute(
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
             CREATE TABLE t (id INT PRIMARY KEY, g INT, name TEXT, m mood, tags TEXT[]); \
             INSERT INTO t SELECT i, i % 97, 'name ' || (i * 7919 % 20000), \
               (ARRAY['sad', 'ok', 'happy'])[1 + i % 3]::mood, ARRAY['a' || (i % 5)] \
               FROM generate_series(1, 20000) i",
        )
        .unwrap();
    }
    let queries = [
        "SELECT id, name FROM t ORDER BY name DESC, id",
        "SELECT m, tags, id FROM t ORDER BY m, id DESC LIMIT 50",
        "SELECT g, id FROM t ORDER BY g NULLS FIRST, id OFFSET 19000",
        "WITH c AS MATERIALIZED (SELECT id, name FROM t WHERE g < 50) \
           SELECT (SELECT count(*) FROM c), (SELECT max(name) FROM c), (SELECT min(id) FROM c)",
        "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 30000) \
           SELECT count(*), sum(n) FROM r",
        "SELECT i FROM generate_series(1, 1000000000) i LIMIT 3",
        // Aggregation, DISTINCT and set operations (sorted, to compare).
        "SELECT name, count(*), sum(id), min(m), string_agg(id::text, ',' ORDER BY id) \
           FROM t GROUP BY name ORDER BY name",
        "SELECT g, count(DISTINCT name), array_agg(id ORDER BY id DESC) FROM t GROUP BY g ORDER BY g",
        "SELECT count(*), sum(id) FROM t",
        "SELECT DISTINCT name, m FROM t ORDER BY name, m",
        "SELECT count(*) FROM (SELECT DISTINCT name FROM t) d",
        "SELECT name FROM t WHERE g < 60 UNION SELECT name FROM t WHERE g > 30 ORDER BY 1",
        "SELECT name, m FROM t INTERSECT SELECT name, m FROM t WHERE id % 2 = 0 ORDER BY 1, 2",
        "SELECT name FROM t EXCEPT SELECT name FROM t WHERE g < 90 ORDER BY 1",
        "SELECT g FROM t INTERSECT ALL SELECT g FROM t WHERE id < 5000 ORDER BY 1",
        "SELECT name FROM t EXCEPT ALL SELECT name FROM t WHERE id % 3 = 0 ORDER BY 1",
        // Joins whose right side is past 64 kB: grace hash joins, block
        // nested loops; NULL keys; left and full joins.
        "SELECT a.id, b.name FROM t a JOIN t b ON a.id = b.id + 1 ORDER BY 1",
        "SELECT a.id, b.id FROM t a LEFT JOIN t b ON a.id = b.g * 1000 ORDER BY 1, 2",
        "SELECT a.k, b.k FROM (SELECT CASE WHEN id % 10 = 0 THEN NULL ELSE id END k FROM t WHERE id < 3000) a \
           FULL JOIN (SELECT CASE WHEN id % 7 = 0 THEN NULL ELSE id END k FROM t WHERE id > 2000) b \
           ON a.k = b.k ORDER BY 1 NULLS FIRST, 2 NULLS FIRST",
        "SELECT count(*), sum(b.id) FROM (SELECT * FROM t WHERE id < 300) a JOIN t b ON b.id < a.id",
        "SELECT a.id, b.id FROM (SELECT * FROM t WHERE id < 30) a LEFT JOIN t b ON b.id < a.id - 25 ORDER BY 1, 2",
        "SELECT a.id, b.id FROM (SELECT * FROM t WHERE id < 20) a FULL JOIN t b ON b.id + 19990 < a.id * 1000 \
           ORDER BY 1 NULLS FIRST, 2",
    ];
    let before = db.scratch_files_made();
    let big = results(&db, "SET work_mem = '64MB'", &queries);
    assert_eq!(
        db.scratch_files_made(),
        before,
        "64 MB is enough: nothing spilled"
    );
    let small = results(&db, "SET work_mem = '64kB'", &queries);
    assert!(
        db.scratch_files_made() > before,
        "64 kB is not: work spilled"
    );
    for (q, (a, b)) in queries.iter().zip(big.iter().zip(&small)) {
        assert_eq!(a, b, "{q}");
    }
    assert_eq!(big[0].len(), 20_000);
    assert_eq!(
        big[4],
        vec![vec![Value::Int(30_000), Value::Int(450_015_000)]]
    );
}

#[test]
fn writes_collect_their_rows_past_work_mem() {
    let db = database();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute("SET work_mem = '64kB'").unwrap();
    s.execute(
        "CREATE TABLE src (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO src SELECT i, repeat('x', i % 40) FROM generate_series(1, 15000) i",
    )
    .unwrap();
    let before = db.scratch_files_made();
    // A write reading its own table collects first: INSERT … SELECT doubles.
    s.execute("INSERT INTO src SELECT id + 15000, v FROM src")
        .unwrap();
    assert!(db.scratch_files_made() > before);
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src"),
        vec![vec![Value::Int(30_000)]]
    );
    s.execute("UPDATE src SET v = v || '!' WHERE id % 2 = 0")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src WHERE v LIKE '%!'"),
        vec![vec![Value::Int(15_000)]]
    );
    let out = s
        .execute("UPDATE src SET id = id + 100000 WHERE id > 29990 RETURNING id")
        .unwrap();
    assert!(matches!(&out[0], Output::Rows { rows, .. } if rows.len() == 10));
    s.execute("CREATE INDEX ON src (v)").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src WHERE v = 'xxx!'"),
        rows(&mut s, "SELECT count(*) FROM src WHERE v || '' = 'xxx!'")
    );
    // (9996 below 29991, and 129993, 129996, 129999.)
    s.execute("DELETE FROM src WHERE id % 3 = 0").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM src"),
        vec![vec![Value::Int(20_001)]]
    );
    // ANALYZE samples a large table, and counts it exactly.
    s.execute("ANALYZE src").unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT analyzed_rows FROM nc_catalog.tables WHERE name = 'src'"
        ),
        vec![vec![Value::Int(20_001)]]
    );
    // Past the sample: the count exact, distinct values estimated.
    s.execute(
        "CREATE TABLE wide (id INT PRIMARY KEY, g INT); \
         INSERT INTO wide SELECT i, i % 100 FROM generate_series(1, 40000) i; ANALYZE wide",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT column_name, rows, distinct_values FROM nc_catalog.statistics \
             WHERE table_name = 'wide' ORDER BY column_name"
        ),
        vec![
            vec![Value::Text("g".into()), Value::Int(40_000), Value::Int(100)],
            vec![
                Value::Text("id".into()),
                Value::Int(40_000),
                Value::Int(40_000)
            ],
        ]
    );
    let e = s.execute("SET work_mem = '1kB'").unwrap_err();
    assert_eq!(e.code, "22023");
}

/// Index builds (PLAN.md §M14.20 W2) sort their entries, past `work_mem`
/// in sorted runs merged: the same index, and the same duplicates found,
/// with 64 kB as with the default — for `CREATE INDEX`, `CREATE UNIQUE
/// INDEX` and `ALTER COLUMN … TYPE`, which rebuilds every index.  On four
/// workers, a large build's merge runs apart from its writes (issue #9):
/// past `work_mem`, and in memory with many entries — a duplicate found by
/// the writer stops the merge; a large spill's records laid out apart.
#[test]
fn index_builds_sort_past_work_mem() {
    for (mem, storage, workers, n) in [
        ("64MB", "row", 1, 20_000),
        ("64kB", "row", 1, 20_000),
        ("64kB", "column", 1, 20_000),
        ("64kB", "row", 4, 20_000),
        ("64MB", "row", 4, 70_000),
        // Spills of over 65,536 entries: their records laid out on the
        // workers.
        ("6MB", "row", 4, 250_000),
    ] {
        let mem = &format!("{mem}, {workers} workers");
        let kv: Arc<dyn Kv> = Arc::new(database());
        let mut s = Session::new(kv.clone());
        s.execute(&format!(
            "SET work_mem = '{}'; SET parallel_workers = {workers}",
            mem.split(',').next().unwrap()
        ))
        .unwrap();
        s.execute(&format!(
            "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT, u INT) WITH (storage = {storage}); \
             INSERT INTO t SELECT i, (i * 7919) % 1000, 'v' || ((i * 31) % 97), \
                    CASE WHEN i % 10 = 0 THEN NULL ELSE i END FROM generate_series(1, {n}) i; \
             CREATE INDEX t_a ON t (a, b); \
             CREATE UNIQUE INDEX t_u ON t (u)"
        ))
        .unwrap();
        // Duplicates in different runs: the 7919-stride keys wrap around.
        let e = s.execute("CREATE UNIQUE INDEX t_bad ON t (a)").unwrap_err();
        assert_eq!(e.code, "23505", "{mem}: {e:?}");
        let e = s.execute("UPDATE t SET u = 5 WHERE id = 6").unwrap_err();
        assert_eq!(e.code, "23505", "{mem}: {e:?}");
        s.execute("ALTER TABLE t ALTER COLUMN a TYPE BIGINT")
            .unwrap_or_else(|e| panic!("{mem} {storage}: {e:?}"));
        for q in [
            "SELECT count(*), sum(id) FROM t WHERE a = 17",
            "SELECT id FROM t WHERE a = 999 AND b = 'v5' ORDER BY id",
            "SELECT count(*) FROM t WHERE u BETWEEN 100 AND 200",
        ] {
            let with_index = rows(&mut s, q);
            let without = rows(
                &mut s,
                &q.replace("FROM t WHERE", "FROM t WHERE id > 0 AND"),
            );
            assert_eq!(with_index, without, "{mem} {storage}: {q}");
        }
        let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
        assert_eq!(
            nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
            Vec::<String>::new(),
            "{mem} {storage}"
        );
    }
}

/// A join whose partitions cannot be split — two keys, each one's rows
/// past `work_mem` — joins them pairwise (a block nested loop) and gives
/// the rows as it finds them: the same rows as in memory, and a `LIMIT`
/// stops it after the pairs it needs (one gathering its rows first tried
/// them all, and held them in scratch space — a fuzzer's `count(*)` over
/// such a join wrote 2.4 GB).
#[test]
fn a_join_its_partitions_cannot_split_gives_its_rows_as_found() {
    let db = database();
    {
        let kv: Arc<dyn Kv> = Arc::new(db.clone());
        let mut s = Session::new(kv);
        s.execute(
            "CREATE TABLE s (id INT PRIMARY KEY, k INT, pad TEXT); \
             INSERT INTO s SELECT i, CASE WHEN i % 50 = 0 THEN NULL ELSE i % 2 END, repeat('p', 100) \
               FROM generate_series(1, 1200) i",
        )
        .unwrap();
    }
    // (Each joins on `pad` too, all alike: the rows carry it, a key's
    // past 64 kB.)
    let queries = [
        "SELECT count(*), sum(a.id + b.id) FROM s a JOIN s b ON a.k = b.k AND a.pad = b.pad",
        "SELECT a.id, b.id FROM s a JOIN s b ON a.k = b.k AND a.pad = b.pad AND b.id > a.id + 1190 ORDER BY 1, 2",
        "SELECT a.id, b.id FROM s a LEFT JOIN s b ON a.k = b.k AND a.pad = b.pad AND b.id > a.id + 1195 ORDER BY 1, 2",
        "SELECT a.id, b.id FROM s a FULL JOIN s b ON a.k = b.k AND a.pad = b.pad AND b.id > a.id + 1195 \
           ORDER BY 1 NULLS FIRST, 2 NULLS FIRST",
        "SELECT count(*), sum(id) FROM s a WHERE EXISTS (SELECT 1 FROM s b WHERE b.k = a.k AND b.pad = a.pad AND b.id > a.id + 1100)",
        "SELECT count(*), sum(id) FROM s a WHERE NOT EXISTS (SELECT 1 FROM s b WHERE b.k = a.k AND b.pad = a.pad AND b.id > a.id + 1100)",
    ];
    let big = results(&db, "SET work_mem = '64MB'", &queries);
    for set in [
        "SET work_mem = '64kB'",
        "SET work_mem = '64kB'; SET batch_size = 0",
    ] {
        let small = results(&db, set, &queries);
        for (q, (a, b)) in queries.iter().zip(big.iter().zip(&small)) {
            assert_eq!(a, b, "{set}: {q}");
        }
    }
    assert_eq!(big[0][0][0], Value::Int(576 * 576 + 600 * 600));
    // Its rows come as found: three wanted, a few pairs tried — not every
    // pair of every partition, as when it gathered them all first.
    let registry = {
        let mut r = nucleus_sql::ext::Registry::builtin();
        r.add(&Ticks).unwrap();
        Arc::new(r)
    };
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::with_registry(kv, registry);
    s.execute("SET work_mem = '64kB'; SET batch_size = 0")
        .unwrap();
    let before = TICKS.load(std::sync::atomic::Ordering::Relaxed);
    let got = rows(
        &mut s,
        "SELECT a.id, b.id FROM s a LEFT JOIN s b ON a.k = b.k AND a.pad = b.pad AND tick(a.id + b.id) LIMIT 3",
    );
    let tried = TICKS.load(std::sync::atomic::Ordering::Relaxed) - before;
    assert_eq!(got.len(), 3);
    assert!(tried < 10_000, "{tried} pairs tried for 3 rows");
    // The partitions spilled: the pairwise join is the one run.
    let before = db.scratch_files_made();
    results(
        &db,
        "SET work_mem = '64kB'; SET batch_size = 0",
        &["SELECT count(*) FROM s a LEFT JOIN s b ON a.k = b.k AND a.pad = b.pad"],
    );
    assert!(db.scratch_files_made() > before, "the partitions spilled");
}

/// The calls of `tick(x)` (true): how many pairs a join tried.
static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Tick;

impl nucleus_sql::ext::ScalarFunction for Tick {
    fn name(&self) -> &str {
        "tick"
    }
    fn arity(&self) -> std::ops::RangeInclusive<usize> {
        1..=1
    }
    fn resolve(
        &self,
        _: &[nucleus_sql::ext::ArgInfo],
    ) -> nucleus_sql::Result<nucleus_sql::ext::Signature> {
        use nucleus_sql::ext::{Arg, Returns, Signature};
        use nucleus_sql::types::DataType;
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Integer)],
            Returns::Type(Some(DataType::Bool)),
        ))
    }
    fn volatility(&self) -> nucleus_sql::ext::Volatility {
        nucleus_sql::ext::Volatility::Volatile
    }
    fn call(
        &self,
        _: &nucleus_sql::ext::CallContext<'_>,
        _: &[Value],
    ) -> nucleus_sql::Result<Value> {
        TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Value::Bool(true))
    }
}

struct Ticks;

impl nucleus_sql::ext::Module for Ticks {
    fn name(&self) -> &str {
        "ticks"
    }
    fn register(&self, r: &mut nucleus_sql::ext::Registry) -> nucleus_sql::Result<()> {
        r.add_function(&["tick"], Arc::new(Tick))
    }
}
