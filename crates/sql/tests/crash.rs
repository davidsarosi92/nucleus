//! The SQL layer under crashes (PLAN.md principle 5): random DDL and DML
//! through [`Session`] on the simulation, a crash at an I/O operation, then
//! reopen.  The database must be in the state after some statement — every
//! statement that returned, perhaps the one in flight — and consistent
//! ([`nucleus_sql::check::check`]): catalog, rows and indexes agree.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Session, TxnStatus, catalog, large};

const STEPS: u64 = 60;

fn options() -> Options {
    Options {
        // Small, so checkpoints (and WAL segment changes) happen inside
        // statements as well as between them.
        checkpoint_wal_bytes: 8 << 10,
        wal_segment_bytes: 4 << 10,
        ..Options::default()
    }
}

fn open(p: &Arc<SimPlatform>) -> nucleus_kv::Result<Database> {
    Database::open_on(p.clone(), Path::new("/db"), options())
}

/// Step `i` of the workload: SQL text, decided by the seed alone (never by
/// earlier results), so a crashed run repeats the dry run up to its crash.
fn step(rng: &SplitMix64, i: u64) -> String {
    let setup = [
        "CREATE TABLE parent (id INT PRIMARY KEY, name TEXT UNIQUE, score INT NOT NULL)",
        "CREATE INDEX parent_score ON parent (score)",
        "INSERT INTO parent SELECT g, 'p' || g, g % 9 FROM generate_series(0, 39) g",
        "CREATE TABLE child (id INT PRIMARY KEY, \
         parent_id INT REFERENCES parent (id) ON DELETE CASCADE, note TEXT)",
        "CREATE TABLE bag (a INT, b TEXT)",
        "CREATE INDEX bag_b ON bag (b)",
    ];
    if let Some(s) = setup.get(i as usize) {
        return s.to_string();
    }
    let k = rng.below(40);
    let n = rng.below(5);
    match rng.below(14) {
        0..=2 => {
            let rows: Vec<String> = (0..=n)
                .map(|j| format!("({}, 'p{}', {})", k + j, (k + j) % 23, rng.below(9)))
                .collect();
            format!("INSERT INTO parent VALUES {}", rows.join(", "))
        }
        3 => format!(
            "UPDATE parent SET score = score + 1 WHERE id % 3 = {}",
            k % 3
        ),
        4 => format!(
            "UPDATE parent SET name = 'q{k}' WHERE id = {}",
            rng.below(40)
        ),
        5 => format!("DELETE FROM parent WHERE id = {k} OR id = {}", k + 50),
        // Some notes are large: stored out of line, in chunks.
        6 | 7 => format!(
            "INSERT INTO child VALUES ({}, {}, {})",
            rng.below(1000),
            rng.below(40),
            if n < 2 {
                format!("repeat('c{k}', {})", 2000 + 20_000 * n)
            } else {
                format!("'c{k}'")
            }
        ),
        8 => {
            let rows: Vec<String> = (0..=n * 3)
                .map(|j| format!("({j}, 'b{}')", k % 7))
                .collect();
            format!("INSERT INTO bag VALUES {}", rows.join(", "))
        }
        9 => format!("DELETE FROM bag WHERE a < {n}"),
        10 => [
            "ALTER TABLE bag ADD COLUMN c INT DEFAULT 5",
            "CREATE INDEX bag_c ON bag (c)",
            "DROP INDEX bag_b",
            "CREATE INDEX bag_b ON bag (b)",
            "ALTER TABLE child ADD COLUMN extra TEXT",
        ][n as usize]
            .to_string(),
        11 => format!(
            "BEGIN; INSERT INTO parent VALUES ({}, 'x{k}', 1); \
             UPDATE parent SET score = 0 WHERE id = {k}; \
             DELETE FROM child WHERE id % 5 = {n}; COMMIT",
            100 + k
        ),
        12 => format!("BEGIN; INSERT INTO bag VALUES ({k}, 'r'); ROLLBACK"),
        13 if n < 2 => format!(
            "UPDATE child SET note = {} WHERE id % 7 = {}",
            if n == 0 {
                "note || 'x'".to_string()
            } else {
                "'short'".into()
            },
            k % 7
        ),
        _ => format!("UPDATE parent SET id = id + 50 WHERE id = {k}"),
    }
}

/// The SQL-visible state: every table definition and its rows.
fn dump(kv: &dyn Kv) -> String {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let mut out = String::new();
    for t in catalog::tables(&*txn).unwrap() {
        out.push_str(&format!("{t:?}\n"));
        let prefix = t.index_prefix(0);
        let end = nucleus_kv::prefix_end(&prefix).unwrap();
        let scan = txn
            .scan(
                std::ops::Bound::Included(&prefix),
                std::ops::Bound::Excluded(&end),
            )
            .unwrap();
        for e in scan {
            let (k, v) = e.unwrap();
            let row = large::read_row(&*txn, &t, &k, &v).unwrap();
            out.push_str(&format!("  {row:?}\n"));
        }
    }
    out
}

/// Runs the workload until it ends or the file system crashes.  Returns
/// the state after each step (index 0: before any) if `record`, how many
/// steps finished before the crash, and whether the crash hit inside a
/// step.
fn workload(p: &Arc<SimPlatform>, seed: u64, record: bool) -> (Vec<String>, u64, bool) {
    let rng = SplitMix64::new(seed);
    let Ok(db) = open(p) else {
        return (Vec::new(), 0, false);
    };
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv.clone());
    let mut states = Vec::new();
    if record {
        states.push(dump(&*kv));
    }
    for i in 0..STEPS {
        let sql = step(&rng, i);
        if s.execute(&sql).is_err() && s.status() != TxnStatus::Idle {
            let _ = s.execute("ROLLBACK");
        }
        if p.sim_fs().crash_count() > 0 {
            return (states, i, true);
        }
        if record {
            states.push(dump(&*kv));
        }
        if i % 9 == 8 && db.checkpoint().is_err() {
            return (states, i + 1, false);
        }
    }
    (states, STEPS, false)
}

#[test]
fn sql_survives_a_crash_at_any_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("sql_crash_at_any_io_point", 2, |seed| {
            let dry = Arc::new(SimPlatform::new(seed));
            let (states, done, _) = workload(&dry, seed, true);
            assert_eq!(done, STEPS);
            let total = dry.sim_fs().op_count();
            // Every operation, unless a change to the workload makes that
            // slow; a stride still lands in every step.
            let stride = (total / 1000).max(1);
            for n in (1..=total).step_by(stride as usize) {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, done, in_step) = workload(&p, seed, false);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                let ctx = format!("seed {seed:#x}, {mode:?} crash at op {n} (step {done})");

                let db = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let kv: Arc<dyn Kv> = Arc::new(db);
                let got = dump(&*kv);
                let done = done as usize;
                let ok = got == states[done] || (in_step && states.get(done + 1) == Some(&got));
                assert!(ok, "{ctx}: recovered a state that no step left:\n{got}");
                let txn = kv.begin(TxnOptions::read_only()).unwrap();
                let problems =
                    nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin())
                        .unwrap();
                assert!(problems.is_empty(), "{ctx}: {problems:#?}");
                drop(txn);
                // And it keeps working.
                let mut s = Session::new(kv);
                s.execute("CREATE TABLE after_crash (a INT PRIMARY KEY)")
                    .unwrap();
                s.execute("INSERT INTO after_crash VALUES (1)").unwrap();
            }
        });
    }
}

#[test]
fn the_workload_does_what_it_should() {
    // A sanity check of the test itself: the dry run builds real data, and
    // the checker finds an index entry removed behind its back.
    let p = Arc::new(SimPlatform::new(3));
    let (states, _, _) = workload(&p, 3, true);
    let last = states.last().unwrap();
    assert!(last.matches("Int(").count() > 20, "{last}");
    let db = open(&p).unwrap();
    let mut txn = db.begin(TxnOptions::default()).unwrap();
    let t = catalog::table(&*txn, "parent").unwrap();
    let prefix = t.index_prefix(t.indexes[0].id);
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    let first = txn
        .scan(
            std::ops::Bound::Included(&prefix),
            std::ops::Bound::Excluded(&end),
        )
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .0;
    txn.delete(&first).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("1 missing"), "{problems:#?}");
}
