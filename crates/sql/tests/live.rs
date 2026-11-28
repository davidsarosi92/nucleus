//! Live queries (PLAN.md §M16.6 S2): `SUBSCRIBE`, `CHANGES`,
//! `UNSUBSCRIBE` — the first result, then each batch of differences a
//! commit brought, at one snapshot, under concurrent writers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::parser::parse;
use nucleus_sql::{Output, Session, Value};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
            Path::new("/db"),
            Options {
                history: 1_000_000,
                ..Options::default()
            },
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

/// The rows without `nc_ts`, rendered: `diff col…`.
fn diffs(s: &mut Session, sql: &str) -> Vec<String> {
    rows(s, sql)
        .into_iter()
        .map(|r| {
            r[1..]
                .iter()
                .map(Value::render)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

fn run(s: &mut Session, sql: &str) {
    s.execute(sql).unwrap();
}

#[test]
fn a_subscription_answers_its_result_then_what_each_commit_changed() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, v text); CREATE TABLE other (x int); \
         INSERT INTO t VALUES (1, 'a'), (2, 'b')",
    );
    let first = rows(
        &mut s,
        "SUBSCRIBE s AS SELECT id, v FROM t WHERE id < 10 ORDER BY id",
    );
    assert_eq!(
        first
            .iter()
            .map(|r| (r[1].render(), r[2].render(), r[3].render()))
            .collect::<Vec<_>>(),
        [
            ("1".into(), "1".into(), "a".into()),
            ("1".into(), "2".into(), "b".into())
        ]
    );
    let ts0 = first[0][0].clone();
    // Nothing since.
    assert!(diffs(&mut s, "CHANGES s").is_empty());
    assert!(diffs(&mut s, "CHANGES s WAIT 20").is_empty());
    // A row in; a row outside the filter; another table.
    run(&mut w, "INSERT INTO t VALUES (3, 'c'), (11, 'z')");
    run(&mut w, "INSERT INTO other VALUES (1)");
    let got = rows(&mut s, "CHANGES s WAIT 1000");
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0][1..].iter().map(Value::render).collect::<Vec<_>>(),
        ["1", "3", "c"]
    );
    assert!(matches!((&got[0][0], &ts0), (Value::Int(a), Value::Int(b)) if a > b));
    // An update: the old row out, the new in, at one snapshot.
    run(&mut w, "UPDATE t SET v = 'B' WHERE id = 2");
    let got = rows(&mut s, "CHANGES s WAIT 1000");
    assert_eq!(
        got.iter()
            .map(|r| r[1..]
                .iter()
                .map(Value::render)
                .collect::<Vec<_>>()
                .join(" "))
            .collect::<Vec<_>>(),
        ["-1 2 b", "1 2 B"]
    );
    assert_eq!(got[0][0], got[1][0]);
    // A delete; two commits at once come as one batch.
    run(&mut w, "DELETE FROM t WHERE id = 1");
    run(&mut w, "INSERT INTO t VALUES (4, 'd')");
    assert_eq!(diffs(&mut s, "CHANGES s WAIT 1000"), ["-1 1 a", "1 4 d"]);
    // Writes that change nothing it answers: no batch.
    run(&mut w, "UPDATE t SET v = 'zz' WHERE id = 11");
    assert!(diffs(&mut s, "CHANGES s WAIT 30").is_empty());
    run(&mut s, "UNSUBSCRIBE s");
    assert_eq!(code(&mut s, "CHANGES s"), "42704");
}

#[test]
fn aggregates_joins_derived_tables_and_limits_follow_their_tables() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE a (id int PRIMARY KEY, g text); \
         CREATE TABLE b (id int PRIMARY KEY, a_id int, n int); \
         INSERT INTO a VALUES (1, 'x'), (2, 'y'); INSERT INTO b VALUES (1, 1, 5), (2, 2, 7)",
    );
    assert_eq!(
        diffs(&mut s, "SUBSCRIBE c AS SELECT count(*) FROM b"),
        ["1 2"]
    );
    assert_eq!(
        diffs(
            &mut s,
            "SUBSCRIBE v AS SELECT g, total FROM (SELECT a.g, sum(b.n) AS total FROM a JOIN b ON b.a_id = a.id GROUP BY a.g) AS totals ORDER BY g"
        ),
        ["1 x 5", "1 y 7"]
    );
    assert_eq!(
        diffs(
            &mut s,
            "SUBSCRIBE top AS SELECT id, n FROM b ORDER BY n DESC LIMIT 1"
        ),
        ["1 2 7"]
    );
    assert_eq!(
        diffs(
            &mut s,
            "SUBSCRIBE sub AS SELECT g FROM a WHERE id IN (SELECT a_id FROM b WHERE n > 6)"
        ),
        ["1 y"]
    );
    // A write to the join's other side, through the derived table.
    run(&mut w, "INSERT INTO b VALUES (3, 1, 10)");
    assert_eq!(diffs(&mut s, "CHANGES c WAIT 1000"), ["-1 2", "1 3"]);
    assert_eq!(diffs(&mut s, "CHANGES v WAIT 1000"), ["-1 x 5", "1 x 15"]);
    assert_eq!(diffs(&mut s, "CHANGES top WAIT 1000"), ["-1 2 7", "1 3 10"]);
    assert_eq!(diffs(&mut s, "CHANGES sub WAIT 1000"), ["1 x"]);
    // The subquery's table only.
    run(&mut w, "UPDATE a SET g = 'X' WHERE id = 1");
    assert_eq!(diffs(&mut s, "CHANGES sub WAIT 1000"), ["-1 x", "1 X"]);
    assert_eq!(diffs(&mut s, "CHANGES v WAIT 1000"), ["-1 x 15", "1 X 15"]);
    assert!(diffs(&mut s, "CHANGES c WAIT 20").is_empty());
}

#[test]
fn duplicates_are_counted() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, v text); INSERT INTO t VALUES (1, 'a'), (2, 'a')",
    );
    assert_eq!(
        diffs(&mut s, "SUBSCRIBE s AS SELECT v FROM t"),
        ["1 a", "1 a"]
    );
    run(&mut w, "INSERT INTO t VALUES (3, 'a'), (4, 'a'), (5, 'b')");
    assert_eq!(diffs(&mut s, "CHANGES s WAIT 1000"), ["2 a", "1 b"]);
    // One more of a row it holds.
    run(&mut w, "INSERT INTO t VALUES (6, 'a')");
    assert_eq!(diffs(&mut s, "CHANGES s WAIT 1000"), ["1 a"]);
    run(&mut w, "DELETE FROM t WHERE v = 'a'");
    assert_eq!(diffs(&mut s, "CHANGES s WAIT 1000"), ["-5 a"]);
    // Several gone and several come at once: each part in the rows' order.
    run(&mut w, "INSERT INTO t VALUES (7, 'c'), (8, 'd'), (9, 'e')");
    assert_eq!(diffs(&mut s, "CHANGES s WAIT 1000"), ["1 c", "1 d", "1 e"]);
    run(
        &mut w,
        "DELETE FROM t WHERE v IN ('b', 'c', 'd', 'e'); INSERT INTO t VALUES (10, 'x'), (11, 'y')",
    );
    let got = diffs(&mut s, "CHANGES s WAIT 1000");
    // (The two statements may come as one batch or two.)
    let all: Vec<String> = if got.len() == 4 {
        let mut more = diffs(&mut s, "CHANGES s WAIT 1000");
        let mut v = got;
        v.append(&mut more);
        v
    } else {
        got
    };
    assert_eq!(all, ["-1 b", "-1 c", "-1 d", "-1 e", "1 x", "1 y"]);
}

#[test]
fn parameters_are_bound_at_subscribe() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, owner text); INSERT INTO t VALUES (1, 'ann'), (2, 'bob')",
    );
    let stmt = parse("SUBSCRIBE mine AS SELECT id FROM t WHERE owner = $1")
        .unwrap()
        .remove(0);
    let Output::Rows { rows, columns } = s.run(&stmt, &[Value::Text("ann".into())]).unwrap() else {
        panic!()
    };
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["nc_ts", "nc_diff", "id"]
    );
    assert_eq!(rows.len(), 1);
    run(&mut w, "INSERT INTO t VALUES (3, 'bob'), (4, 'ann')");
    assert_eq!(diffs(&mut s, "CHANGES mine WAIT 1000"), ["1 4"]);
    // WAIT as a parameter.
    let stmt = parse("CHANGES mine WAIT $1").unwrap().remove(0);
    let Output::Rows { rows, .. } = s.run(&stmt, &[Value::Int(10)]).unwrap() else {
        panic!()
    };
    assert!(rows.is_empty());
    assert_eq!(
        s.run(&stmt, &[Value::Int(-1)])
            .unwrap_err()
            .code
            .to_string(),
        "22023"
    );
}

#[test]
fn what_it_refuses() {
    let kv = db();
    let mut s = Session::new(kv);
    run(&mut s, "CREATE TABLE t (id int PRIMARY KEY, at timestamp)");
    // Answers that change without a commit.
    assert_eq!(
        code(&mut s, "SUBSCRIBE a AS SELECT id FROM t WHERE at > now()"),
        "0A000"
    );
    assert_eq!(code(&mut s, "SUBSCRIBE a AS SELECT random()"), "0A000");
    assert_eq!(
        code(
            &mut s,
            "SUBSCRIBE a AS SELECT id FROM t WHERE at > (SELECT now())"
        ),
        "0A000"
    );
    assert_eq!(code(&mut s, "SUBSCRIBE a AS SELECT current_date"), "0A000");
    // Names.
    run(&mut s, "SUBSCRIBE a AS SELECT id FROM t");
    assert_eq!(code(&mut s, "SUBSCRIBE a AS SELECT id FROM t"), "42710");
    assert_eq!(code(&mut s, "CHANGES nope"), "42704");
    assert_eq!(code(&mut s, "UNSUBSCRIBE nope"), "42704");
    // Not in a transaction block.
    run(&mut s, "BEGIN");
    assert_eq!(code(&mut s, "CHANGES a"), "25001");
    run(&mut s, "ROLLBACK");
    run(&mut s, "BEGIN");
    assert_eq!(code(&mut s, "SUBSCRIBE b AS SELECT 1"), "25001");
    run(&mut s, "ROLLBACK");
    // A session's limit.
    run(&mut s, "UNSUBSCRIBE ALL");
    for i in 0..nucleus_sql::live::PER_SESSION {
        run(&mut s, &format!("SUBSCRIBE s{i} AS SELECT id FROM t"));
    }
    assert_eq!(code(&mut s, "SUBSCRIBE more AS SELECT id FROM t"), "54000");
    run(&mut s, "UNSUBSCRIBE ALL");
    run(&mut s, "SUBSCRIBE more AS SELECT id FROM t");
}

#[test]
fn a_table_dropped_or_out_of_reach_ends_the_subscription() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut admin = Session::new(kv);
    run(
        &mut admin,
        "CREATE TABLE t (id int PRIMARY KEY); CREATE TABLE u (id int PRIMARY KEY); \
         CREATE USER app; GRANT SELECT ON t TO app; GRANT SELECT ON u TO app; INSERT INTO t VALUES (1)",
    );
    s.login("app").unwrap();
    run(&mut admin, "CREATE TABLE hidden (id int)");
    assert_eq!(
        code(&mut s, "SUBSCRIBE h AS SELECT id FROM hidden"),
        "42501"
    );
    assert_eq!(diffs(&mut s, "SUBSCRIBE a AS SELECT id FROM t"), ["1 1"]);
    assert_eq!(
        diffs(&mut s, "SUBSCRIBE b AS SELECT id FROM u"),
        Vec::<String>::new()
    );
    // Its privilege taken back: the next run is refused, and it ends.
    run(&mut admin, "REVOKE SELECT ON t FROM app");
    assert_eq!(code(&mut s, "SELECT id FROM t"), "42501");
    assert_eq!(code(&mut s, "CHANGES a WAIT 1000"), "42501");
    assert_eq!(code(&mut s, "CHANGES a"), "42704");
    // Its table dropped.
    run(&mut admin, "DROP TABLE u");
    assert_eq!(code(&mut s, "CHANGES b WAIT 1000"), "42P01");
    assert_eq!(code(&mut s, "CHANGES b"), "42704");
}

#[test]
fn a_column_added_ends_a_star_subscription() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t VALUES (1)",
    );
    run(&mut s, "SUBSCRIBE a AS SELECT * FROM t");
    run(&mut s, "SUBSCRIBE b AS SELECT id FROM t");
    run(&mut w, "ALTER TABLE t ADD COLUMN v text");
    assert_eq!(code(&mut s, "CHANGES a WAIT 1000"), "55000");
    // The one naming its column goes on.
    assert!(diffs(&mut s, "CHANGES b WAIT 20").is_empty());
    run(&mut w, "INSERT INTO t VALUES (2, 'x')");
    assert_eq!(diffs(&mut s, "CHANGES b WAIT 1000"), ["1 2"]);
}

/// Writers commit rows in their own order while a subscriber applies
/// each batch: after every batch its result is the query's at the
/// batch's commit, and once the writers are done it is the whole.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn under_concurrent_writers_each_batch_is_the_result_at_its_commit() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    run(&mut s, "CREATE TABLE t (w int, i int, PRIMARY KEY (w, i))");
    let (writers, each) = (4, 40);
    let q = "SELECT w, i FROM t WHERE i % 3 <> 1";
    let mut state: HashMap<Vec<String>, i64> = HashMap::new();
    for r in rows(&mut s, &format!("SUBSCRIBE s AS {q}")) {
        *state
            .entry(r[2..].iter().map(Value::render).collect())
            .or_default() += 1;
    }
    let mut check = Session::new(kv.clone());
    let expected_total = writers * (0..each).filter(|i| i % 3 != 1).count();
    let done = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for wi in 0..writers {
            let kv = kv.clone();
            let done = &done;
            scope.spawn(move || {
                let mut w = Session::new(kv);
                for i in 0..each {
                    run(&mut w, &format!("INSERT INTO t VALUES ({wi}, {i})"));
                    if i % 5 == 4 {
                        // Some taken back again.
                        run(
                            &mut w,
                            &format!("DELETE FROM t WHERE w = {wi} AND i = {}", i - 2),
                        );
                        run(&mut w, &format!("INSERT INTO t VALUES ({wi}, {})", i - 2));
                    }
                }
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        let mut batches = 0;
        loop {
            // Read before the wait: once all are done, an empty batch
            // after means nothing is left to come.
            let finished = done.load(std::sync::atomic::Ordering::SeqCst) == writers;
            let got = rows(&mut s, "CHANGES s WAIT 5000");
            if got.is_empty() {
                assert!(finished, "a wait ran out while writers were writing");
                break;
            }
            batches += 1;
            let Value::Int(ts) = got[0][0] else { panic!() };
            for r in &got {
                assert_eq!(r[0], got[0][0], "one snapshot per batch");
                let Value::Int(d) = r[1] else { panic!() };
                let e = state
                    .entry(r[2..].iter().map(Value::render).collect())
                    .or_default();
                *e += d;
                if *e == 0 {
                    state.remove(&r[2..].iter().map(Value::render).collect::<Vec<_>>());
                }
            }
            let mut at: HashMap<Vec<String>, i64> = HashMap::new();
            for r in rows(
                &mut check,
                &format!("SELECT w, i FROM t AS OF COMMIT {ts} WHERE i % 3 <> 1"),
            ) {
                *at.entry(r.iter().map(Value::render).collect()).or_default() += 1;
            }
            assert_eq!(state, at, "the result at commit {ts}");
        }
        assert!(batches >= 2);
    });
    assert_eq!(state.len(), expected_total);
}

#[test]
fn row_level_security_holds_at_every_run() {
    let kv = db();
    let mut admin = Session::new(kv.clone());
    run(
        &mut admin,
        "CREATE TABLE notes (id int PRIMARY KEY, owner text, body text); \
         CREATE USER ann; CREATE USER bob; \
         GRANT SELECT, INSERT ON notes TO ann; GRANT SELECT, INSERT ON notes TO bob; \
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY own ON notes USING (owner = current_user); \
         INSERT INTO notes VALUES (1, 'ann', 'a1'), (2, 'bob', 'b1')",
    );
    let mut ann = Session::new(kv.clone());
    ann.login("ann").unwrap();
    let mut bob = Session::new(kv);
    bob.login("bob").unwrap();
    assert_eq!(
        diffs(&mut ann, "SUBSCRIBE mine AS SELECT body FROM notes"),
        ["1 a1"]
    );
    run(&mut bob, "INSERT INTO notes VALUES (3, 'bob', 'b2')");
    assert!(diffs(&mut ann, "CHANGES mine WAIT 30").is_empty());
    run(&mut admin, "INSERT INTO notes VALUES (4, 'ann', 'a2')");
    assert_eq!(diffs(&mut ann, "CHANGES mine WAIT 1000"), ["1 a2"]);
    // A policy changed: the next run follows it.
    run(
        &mut admin,
        "DROP POLICY own ON notes; CREATE POLICY own ON notes USING (true)",
    );
    assert_eq!(diffs(&mut ann, "CHANGES mine WAIT 1000"), ["1 b1", "1 b2"]);
}

#[test]
fn a_partitioned_table_s_partitions_are_watched() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(
        &mut s,
        "CREATE TABLE ev (k int, v text) PARTITION BY RANGE (k); \
         CREATE TABLE ev_a PARTITION OF ev FOR VALUES FROM (0) TO (10); \
         CREATE TABLE ev_b PARTITION OF ev FOR VALUES FROM (10) TO (20); \
         INSERT INTO ev VALUES (1, 'a')",
    );
    assert_eq!(
        diffs(&mut s, "SUBSCRIBE e AS SELECT k, v FROM ev"),
        ["1 1 a"]
    );
    run(&mut w, "INSERT INTO ev_b VALUES (15, 'b')");
    assert_eq!(diffs(&mut s, "CHANGES e WAIT 1000"), ["1 15 b"]);
}

#[test]
fn a_table_made_again_under_its_name_is_watched_anew() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let mut w = Session::new(kv);
    run(&mut s, "CREATE TABLE t (id int); INSERT INTO t VALUES (1)");
    assert_eq!(diffs(&mut s, "SUBSCRIBE a AS SELECT id FROM t"), ["1 1"]);
    run(
        &mut w,
        "DROP TABLE t; CREATE TABLE t (id int); INSERT INTO t VALUES (2)",
    );
    assert_eq!(diffs(&mut s, "CHANGES a WAIT 1000"), ["-1 1", "1 2"]);
    // Its new rows: the new table's range is the one watched.
    run(&mut w, "INSERT INTO t VALUES (3)");
    assert_eq!(diffs(&mut s, "CHANGES a WAIT 1000"), ["1 3"]);
}

#[test]
fn a_result_and_the_database_s_live_queries_are_limited() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    let rows_most = nucleus_sql::live::ROWS;
    assert_eq!(
        code(
            &mut s,
            &format!(
                "SUBSCRIBE big AS SELECT g FROM generate_series(1, {}) g",
                rows_most + 1
            )
        ),
        "54000"
    );
    run(
        &mut s,
        &format!("SUBSCRIBE big AS SELECT g FROM generate_series(1, {rows_most}) g"),
    );
    run(&mut s, "UNSUBSCRIBE big");
    let mut sessions = Vec::new();
    let mut made = 0;
    while made < nucleus_sql::live::IN_DATABASE {
        let mut t = Session::new(kv.clone());
        for _ in 0..nucleus_sql::live::PER_SESSION.min(nucleus_sql::live::IN_DATABASE - made) {
            run(&mut t, &format!("SUBSCRIBE s{made} AS SELECT 1"));
            made += 1;
        }
        sessions.push(t);
    }
    assert_eq!(code(&mut s, "SUBSCRIBE one AS SELECT 1"), "53400");
    // A session gone takes its own with it.
    sessions.pop();
    run(&mut s, "SUBSCRIBE one AS SELECT 1");
}
