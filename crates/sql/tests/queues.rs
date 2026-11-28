//! Task queues (PLAN.md §M16.5 S5) beyond `slt/99_queues.slt`: workers
//! competing for one queue do every task exactly once with their work,
//! a worker that lost its lease while working cannot commit the work, a
//! crashed worker's task comes back, and taking reads one priority's
//! ready tasks however many are delayed.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session};

fn open(seed: u64) -> Arc<dyn Kv> {
    open_on(&Arc::new(SimPlatform::new(seed)))
}

/// A database on `platform`, whose clock moves when the test moves it.
fn open_on(platform: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::clone(platform) as _,
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

/// `sql`'s rows, each column an integer (NULL as -1; text and others
/// panic).
fn ints(s: &mut Session, sql: &str) -> Vec<Vec<i64>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Int(i) => *i,
                        Value::Null => -1,
                        v => panic!("{sql}: {v:?}"),
                    })
                    .collect()
            })
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Workers take a task (its own transaction), do its work and finish it
/// in one transaction; some fail on their first attempt and are retried.
/// Every task's work is committed exactly once, none lost; the takers'
/// conflicts are resolved by the statement's retry.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn competing_workers_do_every_task_exactly_once() {
    const TASKS: i64 = 200;
    const WORKERS: usize = 4;
    let platform = Arc::new(SimPlatform::new(1));
    let kv = open_on(&platform);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE QUEUE jobs (n INT NOT NULL) WITH (backoff = '1 millisecond'); \
         CREATE TABLE done (n INT PRIMARY KEY, by_worker INT, attempt BIGINT)",
    )
    .unwrap();
    s.execute(&format!(
        "INSERT INTO jobs (n, priority) SELECT g, g % 3 FROM generate_series(1, {TASKS}) g"
    ))
    .unwrap();
    let workers: Vec<_> = (0..WORKERS)
        .map(|w| {
            let kv = Arc::clone(&kv);
            let platform = Arc::clone(&platform);
            std::thread::spawn(move || {
                let mut s = Session::new(kv);
                let mut idle = 0;
                let mut failed = 0;
                while idle < 50 {
                    let got = ints(&mut s, "TAKE id, n FROM jobs");
                    let Some(t) = got.first() else {
                        idle += 1;
                        // (The failed ones' backoff passes.)
                        platform.sim_clock().advance(Duration::from_millis(1));
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    idle = 0;
                    let (id, attempt, n) = (t[0], t[1], t[2]);
                    // Every seventh task fails on its first attempt.
                    if n % 7 == 0 && attempt == 1 {
                        s.execute(&format!(
                            "FAIL TASK {id} OF jobs ATTEMPT {attempt} ERROR 'flaky'"
                        ))
                        .unwrap();
                        failed += 1;
                        continue;
                    }
                    s.execute("BEGIN").unwrap();
                    s.execute(&format!("INSERT INTO done VALUES ({n}, {w}, {attempt})"))
                        .unwrap();
                    s.execute(&format!("FINISH TASK {id} OF jobs ATTEMPT {attempt}"))
                        .unwrap();
                    s.execute("COMMIT").unwrap();
                }
                failed
            })
        })
        .collect();
    let failed: i64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    assert_eq!(failed, TASKS / 7);
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*), count(DISTINCT n), sum(n) FROM done"
        ),
        vec![vec![TASKS, TASKS, TASKS * (TASKS + 1) / 2]]
    );
    assert_eq!(ints(&mut s, "SELECT count(*) FROM jobs"), vec![vec![0]]);
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*) FROM done WHERE n % 7 = 0 AND attempt = 2"
        ),
        vec![vec![TASKS / 7]]
    );
    // The work spread over the workers.
    let by = ints(&mut s, "SELECT count(DISTINCT by_worker) FROM done");
    assert!(by[0][0] > 1, "{by:?}");
    check(&kv, &s);
}

/// A worker whose lease ran out while it worked, the task taken again by
/// another: its finish in a transaction begun before that fails with a
/// conflict (the task changed after its snapshot), one begun after fails
/// at once (`55000`); the second worker's finish commits — one work done.
#[test]
fn a_lost_lease_cannot_commit_its_work() {
    let platform = Arc::new(SimPlatform::new(2));
    let kv = open_on(&platform);
    let mut a = Session::new(Arc::clone(&kv));
    let mut b = Session::new(Arc::clone(&kv));
    a.execute(
        "CREATE QUEUE jobs (n INT); CREATE TABLE done (n INT PRIMARY KEY, by_worker TEXT); \
         INSERT INTO jobs (n) VALUES (1)",
    )
    .unwrap();
    let t = ints(&mut a, "TAKE id, n FROM jobs LEASE '1 millisecond'");
    assert_eq!(t, vec![vec![1, 1, 1]]);
    // A's work begins while it holds the lease.
    a.execute("BEGIN").unwrap();
    a.execute("INSERT INTO done VALUES (1, 'a')").unwrap();
    platform.sim_clock().advance(Duration::from_millis(5));
    // B takes it: the lease ran out.
    assert_eq!(ints(&mut b, "TAKE id FROM jobs"), vec![vec![1, 2]]);
    // A's finish reads its snapshot, where attempt 1 holds it, but its
    // delete meets B's change (or, did it not, its commit would): the
    // work fails with the conflict.
    let e = a
        .execute("FINISH TASK 1 OF jobs ATTEMPT 1")
        .and_then(|_| a.execute("COMMIT"))
        .unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    let _ = a.execute("ROLLBACK");
    // A fresh try of A's: refused at once.
    a.execute("BEGIN").unwrap();
    let e = a.execute("FINISH TASK 1 OF jobs ATTEMPT 1").unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    a.execute("ROLLBACK").unwrap();
    // B's commits.
    b.execute("BEGIN; INSERT INTO done VALUES (1, 'b'); FINISH TASK 1 OF jobs ATTEMPT 2; COMMIT")
        .unwrap();
    match a
        .execute("SELECT by_worker FROM done")
        .unwrap()
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Text("b".into())]]),
        o => panic!("{o:?}"),
    }
    assert_eq!(ints(&mut a, "SELECT count(*) FROM jobs"), vec![vec![0]]);
}

/// A worker that took a task and never finished nor failed it (it
/// crashed): the task comes back when its lease runs out, its attempts
/// counted; past `max_attempts` it is a dead letter.  The queue survives
/// the database closed and opened again.
#[test]
fn a_crashed_workers_task_comes_back() {
    let platform = Arc::new(SimPlatform::new(3));
    let kv = open_on(&platform);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE QUEUE jobs (n INT) WITH (max_attempts = 2, lease = '1 millisecond'); \
         INSERT INTO jobs (n) VALUES (7)",
    )
    .unwrap();
    assert_eq!(ints(&mut s, "TAKE FROM jobs"), vec![vec![1, 1, 7]]);
    drop(s);
    drop(kv);
    let kv = open_on(&platform);
    let mut s = Session::new(Arc::clone(&kv));
    platform.sim_clock().advance(Duration::from_millis(3));
    assert_eq!(ints(&mut s, "TAKE FROM jobs"), vec![vec![1, 2, 7]]);
    platform.sim_clock().advance(Duration::from_millis(3));
    // Its second lease ran out too: a dead letter, nothing to take.
    assert!(ints(&mut s, "TAKE FROM jobs").is_empty());
    assert_eq!(
        ints(&mut s, "SELECT task_id, attempts, n FROM jobs_dead"),
        vec![vec![1, 2, 7]]
    );
    check(&kv, &s);
}

/// Taking reads the ready tasks of one priority at a time through the
/// index: a hundred thousand tasks delayed at the first priority, and as
/// many of another's leased, cost a take little.
#[test]
fn taking_skips_delayed_and_leased_tasks() {
    let kv = open(4);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE QUEUE jobs (n INT)").unwrap();
    s.execute(
        "INSERT INTO jobs (n, priority, available_at) \
         SELECT g, 0, now() + interval '1 day' FROM generate_series(1, 100000) g",
    )
    .unwrap();
    s.execute(
        "INSERT INTO jobs (n, priority, available_at, attempts, taken_at) \
         SELECT g, 1, now() + interval '1 hour', 1, now() FROM generate_series(1, 100000) g",
    )
    .unwrap();
    s.execute("INSERT INTO jobs (n, priority) VALUES (-1, 2), (-2, 5)")
        .unwrap();
    // One read of every task, to compare with.
    let started = Instant::now();
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*) FROM jobs WHERE available_at <= now()"
        ),
        vec![vec![2]]
    );
    let whole = started.elapsed();
    let started = Instant::now();
    for _ in 0..20 {
        s.execute("BEGIN").unwrap();
        let got = ints(&mut s, "TAKE n FROM jobs LIMIT 2");
        assert_eq!(got.iter().map(|r| r[2]).collect::<Vec<_>>(), vec![-1, -2]);
        s.execute("ROLLBACK").unwrap();
    }
    // Twenty takes read less than one read of every task.
    assert!(
        started.elapsed() < whole,
        "{:?} for 20 takes, {whole:?} for one read of all",
        started.elapsed()
    );
}
