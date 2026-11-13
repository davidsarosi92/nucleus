//! Time travel in SQL (PLAN.md §M14.14 TT2–TT4): a read-only
//! transaction, every statement of a session, or one table, at a past
//! time or commit — the catalog as it was then too; how far back it
//! reaches (`nc_catalog.history`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn db(p: &Arc<SimPlatform>, history: Duration) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::clone(p) as Arc<dyn Platform>,
            Path::new("/db"),
            Options {
                history_time: history,
                ..Options::default()
            },
        )
        .unwrap(),
    )
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
    match s.execute(sql) {
        Err(e) => e.code.to_string(),
        Ok(o) => panic!("{sql}: {o:?}"),
    }
}

/// The time now, as SQL writes it.
fn now(s: &mut Session) -> String {
    texts(s, "SELECT now()").remove(0)
}

#[test]
fn a_transaction_reads_a_past_time_and_its_catalog() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::new(db(&p, Duration::from_secs(3600)));
    let tick = |p: &SimPlatform| p.clock().sleep(Duration::from_secs(5));
    s.execute("CREATE TABLE t (id int PRIMARY KEY, v text)")
        .unwrap();
    let mut times = Vec::new();
    for i in 1..=5 {
        tick(&p);
        s.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
            .unwrap();
        times.push(now(&mut s));
    }
    tick(&p);
    s.execute("UPDATE t SET v = 'changed' WHERE id = 2; DELETE FROM t WHERE id = 3")
        .unwrap();
    let after_update = now(&mut s);
    tick(&p);
    // The catalog moves on too: a column added, a table made and dropped.
    s.execute("ALTER TABLE t ADD COLUMN w int DEFAULT 7")
        .unwrap();
    s.execute("CREATE TABLE gone (x int); INSERT INTO gone VALUES (42)")
        .unwrap();
    let with_gone = now(&mut s);
    tick(&p);
    s.execute("DROP TABLE gone").unwrap();
    tick(&p);

    // Each insert's time reads the rows up to it.
    for (i, t) in times.iter().enumerate() {
        s.execute(&format!("BEGIN READ ONLY AS OF TIMESTAMP '{t}'"))
            .unwrap();
        assert_eq!(
            texts(&mut s, "SELECT count(*), max(id) FROM t"),
            [format!("{} {}", i + 1, i + 1)]
        );
        // The table as it was: no `w` yet.
        assert_eq!(
            texts(&mut s, "SELECT * FROM t WHERE id = 1"),
            ["1 v1"],
            "at {t}"
        );
        s.execute("COMMIT").unwrap();
    }
    // A time between commits reads the last before it.
    s.execute(&format!(
        "BEGIN AS OF TIMESTAMP '{}'::timestamptz + interval '2 seconds'",
        times[1]
    ))
    .unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM t"), ["2"]);
    // Read-only: no writes in the past.
    assert_eq!(code(&mut s, "INSERT INTO t VALUES (9, 'x')"), "25006");
    s.execute("ROLLBACK").unwrap();
    // The update's time; and now.
    s.execute(&format!(
        "BEGIN; SET TRANSACTION AS OF TIMESTAMP '{after_update}'"
    ))
    .unwrap();
    assert_eq!(
        texts(&mut s, "SELECT id, v FROM t ORDER BY id"),
        ["1 v1", "2 changed", "4 v4", "5 v5"]
    );
    s.execute("COMMIT").unwrap();
    assert_eq!(
        texts(&mut s, "SELECT id, v, w FROM t WHERE id = 1"),
        ["1 v1 7"]
    );
    // A dropped table, read when it was there.
    assert_eq!(code(&mut s, "SELECT * FROM gone"), "42P01");
    s.execute(&format!("BEGIN AS OF TIMESTAMP '{with_gone}'"))
        .unwrap();
    assert_eq!(texts(&mut s, "SELECT x FROM gone"), ["42"]);
    s.execute("COMMIT").unwrap();
    // The session's setting: every statement at that time.
    s.execute(&format!("SET read_as_of = '{}'", times[2]))
        .unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM t"), ["3"]);
    assert_eq!(texts(&mut s, "SELECT v FROM t WHERE id = 2"), ["v2"]);
    assert_eq!(code(&mut s, "DELETE FROM t"), "25006");
    s.execute("RESET read_as_of").unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM t"), ["4"]);
    // By a commit: the snapshot of a commit named by its number.
    s.execute("BEGIN AS OF COMMIT 1").unwrap();
    assert!(texts(&mut s, "SELECT count(*) FROM t").len() == 1);
    s.execute("COMMIT").unwrap();
}

#[test]
fn a_time_out_of_reach_is_refused() {
    let p = Arc::new(SimPlatform::new(2));
    let mut s = Session::new(db(&p, Duration::from_secs(60)));
    s.execute("CREATE TABLE t (id int)").unwrap();
    let early = now(&mut s);
    for i in 0..5 {
        p.clock().sleep(Duration::from_secs(40));
        s.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    s.execute("VACUUM").unwrap();
    // Past the retention: snapshot too old.
    s.execute(&format!("BEGIN AS OF TIMESTAMP '{early}'"))
        .unwrap();
    assert_eq!(code(&mut s, "SELECT * FROM t"), "72000");
    s.execute("ROLLBACK").unwrap();
    // In the future: refused.
    s.execute("BEGIN AS OF TIMESTAMP now() + interval '1 hour'")
        .unwrap();
    assert_eq!(code(&mut s, "SELECT * FROM t"), "22023");
    s.execute("ROLLBACK").unwrap();
    // NULL, or not a time.
    s.execute("BEGIN AS OF TIMESTAMP NULL").unwrap();
    assert_eq!(code(&mut s, "SELECT 1"), "22023");
    s.execute("ROLLBACK").unwrap();
    // Within it: fine (a minute ago).
    s.execute("BEGIN AS OF TIMESTAMP now() - interval '30 seconds'")
        .unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM t"), ["4"]);
    s.execute("COMMIT").unwrap();
    // How far back: the retention, and the oldest time and commit
    // readable — at or before a minute ago, and readable.
    assert_eq!(
        texts(
            &mut s,
            "SELECT retention, oldest_time <= now() - interval '60 seconds', \
             oldest_commit < newest_commit FROM nc_catalog.history"
        ),
        ["00:01:00 t t"]
    );
    s.execute("BEGIN AS OF TIMESTAMP (SELECT oldest_time FROM nc_catalog.history)")
        .unwrap();
    let oldest = texts(&mut s, "SELECT count(*) FROM t");
    s.execute("COMMIT").unwrap();
    s.execute("BEGIN AS OF COMMIT (SELECT oldest_commit FROM nc_catalog.history)")
        .unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM t"), oldest);
    s.execute("COMMIT").unwrap();
    // A second before it: gone.
    s.execute(
        "BEGIN AS OF TIMESTAMP (SELECT oldest_time - interval '1 second' FROM nc_catalog.history)",
    )
    .unwrap();
    assert_eq!(code(&mut s, "SELECT * FROM t"), "72000");
    s.execute("ROLLBACK").unwrap();
    // Without a retention by time, no time is known.
    let q = Arc::new(SimPlatform::new(3));
    let mut s = Session::new(db(&q, Duration::ZERO));
    s.execute("CREATE TABLE t (id int)").unwrap();
    s.execute("BEGIN AS OF TIMESTAMP now()").unwrap();
    assert_eq!(code(&mut s, "SELECT * FROM t"), "72000");
    s.execute("ROLLBACK").unwrap();
    assert_eq!(
        texts(
            &mut s,
            "SELECT retention IS NULL, oldest_time IS NULL, oldest_commit IS NULL, \
             newest_commit > 0 FROM nc_catalog.history"
        ),
        ["t t t t"]
    );
}

#[test]
fn indexes_answer_for_their_past() {
    // The text index and HNSW, read at a past time, give that time's rows.
    let p = Arc::new(SimPlatform::new(4));
    let kv = db(&p, Duration::from_secs(3600));
    let mut s = Session::new(kv.clone());
    s.execute(
        "CREATE TABLE d (id int PRIMARY KEY, body text, v vector(2)); \
         CREATE INDEX ON d USING text (body); \
         CREATE INDEX ON d USING hnsw (v); \
         INSERT INTO d VALUES (1, 'apple river', '[1,0]'), (2, 'stone cloud', '[0,1]')",
    )
    .unwrap();
    p.clock().sleep(Duration::from_secs(5));
    let then = now(&mut s);
    p.clock().sleep(Duration::from_secs(5));
    s.execute(
        "DELETE FROM d WHERE id = 1; \
         INSERT INTO d VALUES (3, 'apple orchard', '[0.9,0.1]'); \
         UPDATE d SET body = 'apple stone' WHERE id = 2",
    )
    .unwrap();
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    let queries = [
        "SELECT id FROM d WHERE body @@ 'apple' ORDER BY id",
        "SELECT id FROM d ORDER BY v <-> '[1,0]', id LIMIT 1",
        "SELECT id FROM d ORDER BY score(body @@ 'apple') DESC, id LIMIT 5",
    ];
    let now_answers: Vec<Vec<String>> = queries.iter().map(|q| texts(&mut s, q)).collect();
    assert_eq!(now_answers[0], ["2", "3"]);
    s.execute(&format!("BEGIN AS OF TIMESTAMP '{then}'"))
        .unwrap();
    let past: Vec<Vec<String>> = queries.iter().map(|q| texts(&mut s, q)).collect();
    s.execute("COMMIT").unwrap();
    assert_eq!(past[0], ["1"]);
    assert_eq!(past[1], ["1"]);
    // Ranked without a filter: every row of then, the match first.
    assert_eq!(past[2], ["1", "2"]);
    assert_eq!(now_answers[2][0], "2");
}

#[test]
fn a_table_reads_its_past_beside_the_present() {
    let p = Arc::new(SimPlatform::new(5));
    let mut s = Session::new(db(&p, Duration::from_secs(3600)));
    s.execute(
        "CREATE TABLE t (id int PRIMARY KEY, v text); \
         INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'); \
         CREATE TABLE gone (x int, y text); INSERT INTO gone VALUES (1, 'one'), (2, 'two')",
    )
    .unwrap();
    p.clock().sleep(Duration::from_secs(5));
    let then = now(&mut s);
    p.clock().sleep(Duration::from_secs(5));
    s.execute(
        "UPDATE t SET v = 'B' WHERE id = 2; DELETE FROM t WHERE id = 3; \
         INSERT INTO t VALUES (4, 'd'); DROP TABLE gone; \
         ALTER TABLE t ADD COLUMN w int",
    )
    .unwrap();
    // The table then, beside the table now: what changed.
    assert_eq!(
        texts(
            &mut s,
            &format!(
                "SELECT coalesce(n.id, o.id), o.v, n.v \
                 FROM t n FULL JOIN t AS OF TIMESTAMP '{then}' o ON n.id = o.id \
                 WHERE n.v IS DISTINCT FROM o.v ORDER BY 1"
            )
        ),
        ["2 b B", "3 c NULL", "4 NULL d"]
    );
    // SQL:2011's words; the table's columns of then.
    assert_eq!(
        texts(
            &mut s,
            &format!("SELECT * FROM t FOR SYSTEM_TIME AS OF '{then}' ORDER BY id")
        ),
        ["1 a", "2 b", "3 c"]
    );
    // A deleted row put back, in a transaction that writes the present.
    s.execute(&format!(
        "INSERT INTO t (id, v) SELECT id, v FROM t AS OF TIMESTAMP '{then}' WHERE id = 3"
    ))
    .unwrap();
    assert_eq!(texts(&mut s, "SELECT v FROM t WHERE id = 3"), ["c"]);
    // A dropped table made again from its past.
    s.execute(&format!(
        "CREATE TABLE gone AS SELECT * FROM gone AS OF TIMESTAMP '{then}'"
    ))
    .unwrap();
    assert_eq!(
        texts(&mut s, "SELECT x, y FROM gone ORDER BY x"),
        ["1 one", "2 two"]
    );
    // EXPLAIN names the snapshot.
    let plan = texts(
        &mut s,
        &format!("EXPLAIN SELECT * FROM t AS OF TIMESTAMP '{then}'"),
    );
    assert!(plan.iter().any(|l| l.contains("As Of commit")), "{plan:?}");
    // By a commit, and an alias.
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM t AS OF COMMIT 2 AS x"),
        ["3"]
    );
    // Not a WITH query.
    assert_eq!(
        code(
            &mut s,
            &format!("WITH w AS (SELECT 1) SELECT * FROM w AS OF TIMESTAMP '{then}'")
        ),
        "0A000"
    );
}

#[test]
fn reading_the_past_needs_the_privilege_now() {
    let p = Arc::new(SimPlatform::new(6));
    let mut s = Session::new(db(&p, Duration::from_secs(3600)));
    s.execute(
        "CREATE TABLE t (id int); INSERT INTO t VALUES (1); \
         CREATE USER app; GRANT SELECT ON t TO app",
    )
    .unwrap();
    p.clock().sleep(Duration::from_secs(5));
    let then = now(&mut s);
    p.clock().sleep(Duration::from_secs(5));
    s.execute("REVOKE SELECT ON t FROM app").unwrap();
    s.execute("SET ROLE app").unwrap();
    // Granted then, not now: refused.
    assert_eq!(
        code(&mut s, &format!("SELECT * FROM t AS OF TIMESTAMP '{then}'")),
        "42501"
    );
    s.execute("RESET ROLE").unwrap();
    // Granted now, not then: refused too.
    s.execute("CREATE TABLE u (id int); INSERT INTO u VALUES (1)")
        .unwrap();
    p.clock().sleep(Duration::from_secs(5));
    let before_grant = now(&mut s);
    p.clock().sleep(Duration::from_secs(5));
    s.execute("GRANT SELECT ON u TO app; SET ROLE app").unwrap();
    assert_eq!(texts(&mut s, "SELECT count(*) FROM u"), ["1"]);
    assert_eq!(
        code(
            &mut s,
            &format!("SELECT * FROM u AS OF TIMESTAMP '{before_grant}'")
        ),
        "42501"
    );
}

/// A row of the logged workload: `k`, the text, the vector.
type Row4 = (i64, String, [f64; 4]);

fn vector(v: &[f64; 4]) -> String {
    format!("[{},{},{},{}]", v[0], v[1], v[2], v[3])
}

fn ids(rows: Vec<String>) -> Vec<i64> {
    rows.iter().map(|r| r.parse().unwrap()).collect()
}

#[test]
fn a_logged_workload_reads_back_at_random_times_through_every_index() {
    // Rounds of inserts, updates and deletes, the clock moving between
    // them, each round's state logged with its time; then queries at
    // random times — a whole block's and a table's — through a B-tree
    // index (made halfway: the past before it has none), the text index,
    // HNSW and a column-stored table, against the log.  Upkeep, vacuums
    // and a reopen along the way.
    use nucleus_platform::{Rng, SplitMix64, harness};
    use std::collections::BTreeMap;
    harness::run("sql_time_travel_workload", 2, |seed| {
        let p = Arc::new(SimPlatform::new(seed));
        let rng = SplitMix64::new(seed);
        let mut kv = db(&p, Duration::from_secs(3600));
        let mut s = Session::new(kv.clone());
        s.execute(
            "CREATE TABLE d (id int PRIMARY KEY, k int, body text, v vector(4)); \
             CREATE INDEX ON d USING text (body); \
             CREATE INDEX ON d USING hnsw (v); \
             CREATE TABLE c (id int PRIMARY KEY, k int) WITH (storage = column)",
        )
        .unwrap();
        let mut model: BTreeMap<i64, Row4> = BTreeMap::new();
        let mut log: Vec<(String, BTreeMap<i64, Row4>)> = Vec::new();
        let mut gaps = Vec::new(); // the sleep after each round, in µs
        let mut next_id = 0i64;
        let word = |rng: &SplitMix64| format!("w{}", rng.below(10));
        let f = |rng: &SplitMix64| (rng.below(201) as f64 - 100.0) / 100.0;
        for round in 0..80 {
            let mut sql = Vec::new();
            for _ in 0..1 + rng.below(6) {
                let pick = (!model.is_empty()).then(|| {
                    *model
                        .keys()
                        .nth(rng.below(model.len() as u64) as usize)
                        .unwrap()
                });
                let k = rng.below(10) as i64;
                let body = format!("{} {} {}", word(&rng), word(&rng), word(&rng));
                let v = [f(&rng), f(&rng), f(&rng), f(&rng)];
                match (rng.below(5), pick) {
                    (0, Some(id)) => {
                        sql.push(format!("DELETE FROM d WHERE id = {id}"));
                        sql.push(format!("DELETE FROM c WHERE id = {id}"));
                        model.remove(&id);
                    }
                    (1, Some(id)) => {
                        sql.push(format!(
                            "UPDATE d SET k = {k}, body = '{body}', v = '{}' WHERE id = {id}",
                            vector(&v)
                        ));
                        sql.push(format!("UPDATE c SET k = {k} WHERE id = {id}"));
                        model.insert(id, (k, body, v));
                    }
                    _ => {
                        next_id += 1;
                        let id = next_id;
                        sql.push(format!(
                            "INSERT INTO d VALUES ({id}, {k}, '{body}', '{}')",
                            vector(&v)
                        ));
                        sql.push(format!("INSERT INTO c VALUES ({id}, {k})"));
                        model.insert(id, (k, body, v));
                    }
                }
            }
            // One transaction a round.
            s.execute(&format!("BEGIN; {}; COMMIT", sql.join("; ")))
                .unwrap();
            log.push((now(&mut s), model.clone()));
            if round == 40 {
                s.execute("CREATE INDEX d_k ON d (k)").unwrap();
                log.push((now(&mut s), model.clone()));
                gaps.push(1);
            }
            if round % 10 == 9 {
                nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
                nucleus_sql::text::index::upkeep_now(&kv).unwrap();
            }
            if round % 15 == 14 {
                s.execute("VACUUM").unwrap();
            }
            if round == 55 {
                // Closed and opened again: the times and indexes survive.
                drop(s);
                drop(kv);
                kv = db(&p, Duration::from_secs(3600));
                s = Session::new(kv.clone());
            }
            let gap = 1 + rng.below(30_000_000);
            p.clock().sleep(Duration::from_micros(gap));
            gaps.push(gap);
        }
        let mut found = 0usize;
        let mut wanted = 0usize;
        for q in 0..150 {
            let i = rng.below(log.len() as u64) as usize;
            let off = rng.below(gaps[i]);
            let (t, m) = &log[i];
            let at = format!("'{t}'::timestamptz + interval '{off} microseconds'");
            let whole = q % 2 == 0;
            let from = |table: &str| {
                if whole {
                    table.to_string()
                } else {
                    format!("{table} AS OF TIMESTAMP {at}")
                }
            };
            if whole {
                s.execute(&format!("BEGIN READ ONLY AS OF TIMESTAMP {at}"))
                    .unwrap();
            }
            let what = format!("query {q} at round {i} + {off} µs, whole {whole}");
            // The B-tree index (when it existed), the text index, counts.
            let k = rng.below(10) as i64;
            assert_eq!(
                ids(texts(
                    &mut s,
                    &format!("SELECT id FROM {} WHERE k = {k} ORDER BY id", from("d"))
                )),
                m.iter()
                    .filter(|(_, r)| r.0 == k)
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                "{what}: k = {k}"
            );
            let w = format!("w{}", rng.below(10));
            assert_eq!(
                ids(texts(
                    &mut s,
                    &format!(
                        "SELECT id FROM {} WHERE body @@ '{w}' ORDER BY id",
                        from("d")
                    )
                )),
                m.iter()
                    .filter(|(_, r)| r.1.split(' ').any(|x| x == w))
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                "{what}: {w}"
            );
            let mut by_k: BTreeMap<i64, usize> = BTreeMap::new();
            for r in m.values() {
                *by_k.entry(r.0).or_default() += 1;
            }
            assert_eq!(
                texts(
                    &mut s,
                    &format!(
                        "SELECT k, count(*) FROM {} GROUP BY k ORDER BY k",
                        from("c")
                    )
                ),
                by_k.iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>(),
                "{what}: the column table"
            );
            // HNSW: only rows of then, as many as asked, the nearest.
            let target = [f(&rng), f(&rng), f(&rng), f(&rng)];
            let got = ids(texts(
                &mut s,
                &format!(
                    "SELECT id FROM {} ORDER BY v <-> '{}' LIMIT 5",
                    from("d"),
                    vector(&target)
                ),
            ));
            let mut exact: Vec<(f64, i64)> = m
                .iter()
                .map(|(id, r)| {
                    let d: f64 =
                        r.2.iter()
                            .zip(&target)
                            .map(|(a, b)| (a - b) * (a - b))
                            .sum();
                    (d, *id)
                })
                .collect();
            exact.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let exact: Vec<i64> = exact.iter().take(5).map(|(_, id)| *id).collect();
            assert_eq!(got.len(), exact.len(), "{what}: {got:?} {exact:?}");
            assert!(
                got.iter().all(|id| m.contains_key(id)),
                "{what}: {got:?} not all of then"
            );
            found += got.iter().filter(|id| exact.contains(id)).count();
            wanted += exact.len();
            if whole {
                s.execute("COMMIT").unwrap();
            }
        }
        assert!(
            found * 100 >= wanted * 95,
            "HNSW at past times: {found} of {wanted}"
        );
    });
}
