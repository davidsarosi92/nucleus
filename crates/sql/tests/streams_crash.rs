//! Streams, task queues and changefeeds under crashes (PLAN.md §M16.5
//! S7, principle 5): a workload of appends, consumers that write what
//! they consume and commit it with their position, workers that take
//! tasks and finish them with their work — some abandoning a task, some
//! failing one, leases running out — on the simulation, crashed at an
//! I/O operation and reopened.  Whatever the crash hit:
//!
//! - a stream's numbers stay dense from its first;
//! - every entry a group consumed was written once, and none it did not
//!   consume (consume-write-commit exactly once);
//! - every task is in exactly one place — waiting, done (its work written
//!   once), or a dead letter — and none was lost: each one an insert
//!   that returned is there (the one in flight perhaps);
//! - a changefeed holds exactly its table's committed changes, dense;
//! - the database is consistent ([`nucleus_sql::check::check`]), and the
//!   work goes on to the end with the same guarantees.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session, TxnStatus, Value};

const STEPS: u64 = 70;

fn options() -> Options {
    Options {
        // Small, so checkpoints and WAL segment changes happen inside
        // steps as well as between them.
        checkpoint_wal_bytes: 8 << 10,
        wal_segment_bytes: 4 << 10,
        ..Options::default()
    }
}

fn open(p: &Arc<SimPlatform>) -> nucleus_kv::Result<Database> {
    Database::open_on(p.clone(), Path::new("/db"), options())
}

const SETUP: [&str; 5] = [
    "CREATE STREAM events (n INT NOT NULL)",
    // No keys: a row written twice shows.
    "CREATE TABLE done (seq BIGINT NOT NULL, n INT NOT NULL)",
    "CREATE QUEUE jobs (n INT NOT NULL) WITH (lease = '10 seconds', max_attempts = 2, backoff = '1 second')",
    "CREATE TABLE work (n INT NOT NULL)",
    "CREATE CHANGEFEED work_feed ON work",
];

fn rows(s: &mut Session, sql: &str) -> nucleus_sql::Result<Vec<Vec<Value>>> {
    Ok(match s.execute(sql)?.pop() {
        Some(Output::Rows { rows, .. }) => rows,
        _ => Vec::new(),
    })
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn one(s: &mut Session, sql: &str) -> i64 {
    int(&rows(s, sql).unwrap()[0][0])
}

fn ints(s: &mut Session, sql: &str) -> Vec<i64> {
    rows(s, sql).unwrap().iter().map(|r| int(&r[0])).collect()
}

/// A consumer's batch: its entries consumed and written, committed
/// together — or, when `keep` is false, rolled back (a consumer that died
/// before its commit).
fn consume(s: &mut Session, limit: u64, keep: bool) -> nucleus_sql::Result<()> {
    s.execute("BEGIN")?;
    let got = rows(
        s,
        &format!("CONSUME seq, n FROM events GROUP g LIMIT {limit}"),
    )?;
    if !got.is_empty() {
        let values: Vec<String> = got
            .iter()
            .map(|r| format!("({}, {})", int(&r[0]), int(&r[1])))
            .collect();
        s.execute(&format!("INSERT INTO done VALUES {}", values.join(", ")))?;
    }
    s.execute(if keep { "COMMIT" } else { "ROLLBACK" })?;
    Ok(())
}

/// What a worker does with a task it took: finish it with its work, fail
/// it, or nothing (it died; the lease runs out).
#[derive(Clone, Copy)]
enum Fate {
    Finish,
    Fail,
    Abandon,
}

fn work(s: &mut Session, fate: Fate) -> nucleus_sql::Result<()> {
    let got = rows(s, "TAKE n FROM jobs")?;
    let Some(task) = got.first() else {
        return Ok(());
    };
    let (id, attempt, n) = (int(&task[0]), int(&task[1]), int(&task[2]));
    match fate {
        Fate::Finish => {
            s.execute("BEGIN")?;
            s.execute(&format!("INSERT INTO work VALUES ({n})"))?;
            s.execute(&format!("FINISH TASK {id} OF jobs ATTEMPT {attempt}"))?;
            s.execute("COMMIT")?;
        }
        Fate::Fail => {
            s.execute(&format!(
                "FAIL TASK {id} OF jobs ATTEMPT {attempt} ERROR 'step failed'"
            ))?;
        }
        Fate::Abandon => {}
    }
    Ok(())
}

/// What a run did: steps finished, whether the crash hit inside one, the
/// tasks whose inserts returned, and the in-flight step's.
#[derive(Default)]
struct Run {
    done: u64,
    in_step: bool,
    inserted: BTreeSet<i64>,
    pending: BTreeSet<i64>,
}

/// Step `i`: decided by the seed and `i` alone, so a crashed run repeats
/// the dry run up to its crash (the simulation is deterministic).
fn step(p: &SimPlatform, s: &mut Session, rng: &SplitMix64, i: u64, run: &mut Run) {
    run.pending.clear();
    let r = if let Some(sql) = SETUP.get(i as usize) {
        s.execute(sql).map(|_| ())
    } else {
        let k = rng.below(4) + 1;
        match rng.below(13) {
            0..=2 => s
                .execute(&format!(
                    "INSERT INTO events (n) SELECT g FROM generate_series({}, {}) g",
                    i * 10,
                    i * 10 + k
                ))
                .map(|_| ()),
            3 => {
                let tasks: Vec<i64> = (0..k as i64).map(|j| i as i64 * 10 + j).collect();
                run.pending = tasks.iter().copied().collect();
                let values: Vec<String> = tasks.iter().map(|n| format!("({n})")).collect();
                let r = s.execute(&format!(
                    "INSERT INTO jobs (n) VALUES {}",
                    values.join(", ")
                ));
                if r.is_ok() {
                    run.inserted.append(&mut run.pending);
                }
                r.map(|_| ())
            }
            4..=6 => consume(s, k, true),
            7 => consume(s, k, false),
            8 | 9 => work(s, Fate::Finish),
            10 | 11 => work(
                s,
                if rng.below(2) == 0 {
                    Fate::Fail
                } else {
                    Fate::Abandon
                },
            ),
            _ => {
                // Time passes: leases and backoffs run out.
                p.sim_clock().advance(Duration::from_secs(4 * k));
                Ok(())
            }
        }
    };
    if r.is_err() && s.status() != TxnStatus::Idle {
        let _ = s.execute("ROLLBACK");
    }
}

/// Runs the workload until it ends or the file system crashes.
fn workload(p: &Arc<SimPlatform>, seed: u64) -> Run {
    let rng = SplitMix64::new(seed);
    let mut run = Run::default();
    let Ok(db) = open(p) else {
        return run;
    };
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    for i in 0..STEPS {
        // The in-flight insert's tasks are known before it runs.
        step(p, &mut s, &rng, i, &mut run);
        if p.sim_fs().crash_count() > 0 {
            run.done = i;
            run.in_step = true;
            return run;
        }
        if i % 9 == 8 && db.checkpoint().is_err() {
            run.done = i + 1;
            return run;
        }
    }
    run.done = STEPS;
    run
}

/// The guarantees, on what the database holds now.  `inserted`: tasks
/// that must be there; `maybe`: tasks that may.
fn guarantees(s: &mut Session, ctx: &str, inserted: &BTreeSet<i64>, maybe: &BTreeSet<i64>) {
    if rows(
        s,
        "SELECT 1 FROM nc_catalog.tables WHERE name = 'work_feed'",
    )
    .unwrap()
    .is_empty()
    {
        // The crash came before the setup was done.
        return;
    }
    // A stream's numbers: dense from 1 (nothing trims these).
    let seqs = ints(s, "SELECT seq FROM events ORDER BY seq");
    assert!(
        seqs.iter().enumerate().all(|(i, &q)| q == i as i64 + 1),
        "{ctx}: events' seqs not dense: {seqs:?}"
    );
    // Consumed exactly once: done holds entries 1..=position, each once,
    // as they are in the stream.
    let position = one(s, "SELECT stream_position('events', 'g')");
    let written = ints(s, "SELECT seq FROM done ORDER BY seq");
    let want: Vec<i64> = (1..=position).collect();
    assert_eq!(
        written, want,
        "{ctx}: consumed entries written ≠ 1..={position}"
    );
    assert_eq!(
        one(
            s,
            "SELECT count(*) FROM done d JOIN events e ON e.seq = d.seq AND e.n = d.n"
        ),
        position,
        "{ctx}: an entry written as it was not"
    );
    // Every task in exactly one place, none lost, none made up.
    let waiting = ints(s, "SELECT n FROM jobs");
    let finished = ints(s, "SELECT n FROM work");
    let dead = ints(s, "SELECT n FROM jobs_dead");
    let mut all: Vec<i64> = waiting
        .iter()
        .chain(&finished)
        .chain(&dead)
        .copied()
        .collect();
    all.sort_unstable();
    let places = all.len();
    all.dedup();
    assert_eq!(
        places,
        all.len(),
        "{ctx}: a task in two places or done twice: waiting {waiting:?}, done {finished:?}, dead {dead:?}"
    );
    let all: BTreeSet<i64> = all.into_iter().collect();
    assert!(
        all.is_superset(inserted),
        "{ctx}: tasks lost: {:?}",
        inserted.difference(&all).collect::<Vec<_>>()
    );
    let allowed: BTreeSet<i64> = inserted.union(maybe).copied().collect();
    assert!(
        all.is_subset(&allowed),
        "{ctx}: tasks never inserted: {:?}",
        all.difference(&allowed).collect::<Vec<_>>()
    );
    // A dead letter for each task that died, with its attempts.
    assert_eq!(
        one(s, "SELECT count(*) FROM jobs_dead WHERE attempts <> 2"),
        0,
        "{ctx}: a dead letter before its last attempt"
    );
    // The changefeed: work's inserts, each once, dense.
    let feed = ints(s, "SELECT seq FROM work_feed ORDER BY seq");
    assert_eq!(
        feed,
        (1..=finished.len() as i64).collect::<Vec<_>>(),
        "{ctx}: work_feed ≠ work's inserts"
    );
}

/// After a crash and a reopening: the work goes on to its end — every
/// entry consumed, every task done or dead — with the guarantees kept.
fn finish(p: &SimPlatform, s: &mut Session, ctx: &str, inserted: &BTreeSet<i64>) {
    loop {
        let before = one(s, "SELECT stream_position('events', 'g')");
        consume(s, 50, true).unwrap();
        if one(s, "SELECT stream_position('events', 'g')") == before {
            break;
        }
    }
    for _ in 0..1000 {
        if one(s, "SELECT count(*) FROM jobs") == 0 {
            break;
        }
        p.sim_clock().advance(Duration::from_secs(60));
        work(s, Fate::Finish).unwrap();
    }
    assert_eq!(one(s, "SELECT count(*) FROM jobs"), 0, "{ctx}: tasks left");
    let none = BTreeSet::new();
    let mut all = inserted.clone();
    // What the crash left in doubt is decided now.
    all.extend(ints(s, "SELECT n FROM work"));
    all.extend(ints(s, "SELECT n FROM jobs_dead"));
    guarantees(s, ctx, &all, &none);
    assert_eq!(
        one(s, "SELECT stream_position('events', 'g')"),
        one(s, "SELECT coalesce(max(seq), 0) FROM events"),
        "{ctx}: entries left unconsumed"
    );
}

#[test]
fn streams_queues_and_changefeeds_survive_a_crash_at_any_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("streams_crash_at_any_io_point", 2, |seed| {
            let dry = Arc::new(SimPlatform::new(seed));
            let run = workload(&dry, seed);
            assert_eq!(run.done, STEPS);
            let total = dry.sim_fs().op_count();
            {
                // The dry run itself, to its end.
                let kv: Arc<dyn Kv> = Arc::new(open(&dry).unwrap());
                let mut s = Session::new(kv);
                guarantees(&mut s, "dry run", &run.inserted, &BTreeSet::new());
                finish(&dry, &mut s, "dry run", &run.inserted);
            }
            let stride = (total / 600).max(1);
            for n in (1..=total).step_by(stride as usize) {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let run = workload(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                let ctx = format!(
                    "seed {seed:#x}, {mode:?} crash at op {n} (step {}{})",
                    run.done,
                    if run.in_step { ", inside it" } else { "" }
                );
                let db = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let kv: Arc<dyn Kv> = Arc::new(db);
                {
                    let txn = kv.begin(TxnOptions::read_only()).unwrap();
                    let problems =
                        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin())
                            .unwrap();
                    assert!(problems.is_empty(), "{ctx}: {problems:#?}");
                }
                let mut s = Session::new(kv);
                let maybe = if run.in_step {
                    run.pending.clone()
                } else {
                    BTreeSet::new()
                };
                guarantees(&mut s, &ctx, &run.inserted, &maybe);
                if rows(
                    &mut s,
                    "SELECT 1 FROM nc_catalog.tables WHERE name = 'work_feed'",
                )
                .unwrap()
                .is_empty()
                {
                    continue;
                }
                finish(&p, &mut s, &ctx, &run.inserted);
            }
        });
    }
}

#[test]
#[cfg_attr(
    target_os = "wasi",
    ignore = "WASI aborts on a panic: the broken cases' panics cannot be caught"
)]
fn the_workload_does_what_it_should() {
    // A sanity check of the test itself: the dry run consumes, finishes,
    // fails into dead letters and abandons, and the guarantees catch a
    // task finished without its work and an entry written twice.
    let p = Arc::new(SimPlatform::new(3));
    let run = workload(&p, 3);
    let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
    let mut s = Session::new(kv);
    assert!(one(&mut s, "SELECT count(*) FROM done") > 5);
    assert!(one(&mut s, "SELECT count(*) FROM work") > 2);
    assert!(run.inserted.len() > 5, "{}", run.inserted.len());
    finish(&p, &mut s, "sanity", &run.inserted);
    assert!(
        one(&mut s, "SELECT count(*) FROM jobs_dead") > 0,
        "no dead letters"
    );
    for broken in [
        "INSERT INTO done SELECT seq, n FROM done LIMIT 1",
        "DELETE FROM work WHERE n = (SELECT min(n) FROM work)",
    ] {
        let p2 = Arc::new(SimPlatform::new(3));
        let run = workload(&p2, 3);
        let kv: Arc<dyn Kv> = Arc::new(open(&p2).unwrap());
        let mut s = Session::new(kv);
        s.execute(broken).unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guarantees(&mut s, "broken", &run.inserted, &BTreeSet::new())
        }));
        assert!(caught.is_err(), "not caught: {broken}");
    }
}
