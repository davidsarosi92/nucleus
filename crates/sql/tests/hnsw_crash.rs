//! HNSW indexes under crashes (PLAN.md principle 5, §M14.12 V4b): writes
//! to a table with an HNSW index — inserts, vectors changed, deletes,
//! rollbacks — and the index's upkeep (seals and folds) on the
//! simulation, a crash at an I/O operation, then reopen.  The rows must
//! be those after some step, the integrity check must find the index
//! whole (every row in the graph, a sealed object or the delta), a search
//! must give the exact answer at a wide `ef`, and the upkeep must go on.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::{Output, Session, TxnStatus, catalog};

const STEPS: u64 = 40;

fn options() -> Options {
    Options {
        // Small, so checkpoints happen inside the upkeep's transactions.
        checkpoint_wal_bytes: 16 << 10,
        wal_segment_bytes: 8 << 10,
        ..Options::default()
    }
}

fn open(p: &Arc<SimPlatform>) -> nucleus_kv::Result<Database> {
    Database::open_on(p.clone(), Path::new("/db"), options())
}

fn vector(rng: &SplitMix64) -> String {
    let c: Vec<String> = (0..4).map(|_| format!("{}", rng.below(41))).collect();
    format!("'[{}]'", c.join(","))
}

/// Step `i`: SQL, or `None` for the upkeep — decided by the seed alone.
fn step(rng: &SplitMix64, i: u64) -> Option<String> {
    let setup = [
        "CREATE TABLE items (id INT PRIMARY KEY, v vector(4), tag INT)",
        "INSERT INTO items SELECT g, ('[' || (g % 13) || ',' || (g % 7) || ',' || (g % 5) \
         || ',' || (g % 3) || ']')::vector, g % 4 FROM generate_series(1, 150) g",
        "CREATE INDEX items_v ON items USING hnsw (v) WITH (m = 4, ef_construction = 8)",
    ];
    if let Some(s) = setup.get(i as usize) {
        return Some(s.to_string());
    }
    let k = 1 + rng.below(200);
    Some(match rng.below(9) {
        0 | 1 => {
            let rows: Vec<String> = (0..=rng.below(6))
                .map(|j| format!("({}, {}, {})", 1000 + k * 10 + j, vector(rng), j % 4))
                .collect();
            format!(
                "INSERT INTO items VALUES {} ON CONFLICT DO NOTHING",
                rows.join(", ")
            )
        }
        2 => format!(
            "UPDATE items SET v = {} WHERE id % 17 = {}",
            vector(rng),
            k % 17
        ),
        3 => format!("UPDATE items SET tag = tag + 1 WHERE id % 11 = {}", k % 11),
        4 => format!("DELETE FROM items WHERE id % 19 = {}", k % 19),
        5 => format!("UPDATE items SET v = NULL WHERE id = {k}"),
        6 => format!(
            "BEGIN; INSERT INTO items VALUES ({}, {}, 0); ROLLBACK",
            5000 + k,
            vector(rng)
        ),
        _ => return None,
    })
}

/// The rows of every table.
fn dump(kv: &dyn Kv) -> String {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let mut out = String::new();
    for t in catalog::tables(&*txn).unwrap() {
        out.push_str(&format!("{}\n", t.name));
        let prefix = t.index_prefix(0);
        let end = nucleus_kv::prefix_end(&prefix).unwrap();
        for e in txn
            .scan(
                std::ops::Bound::Included(&prefix),
                std::ops::Bound::Excluded(&end),
            )
            .unwrap()
        {
            let (k, v) = e.unwrap();
            out.push_str(&format!("  {k:?} {v:?}\n"));
        }
    }
    out
}

/// Runs the workload until it ends or the file system crashes: the state
/// after each step (if `record`), the steps finished, whether the crash
/// hit inside one.
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
        match step(&rng, i) {
            Some(sql) => {
                if s.execute(&sql).is_err() && s.status() != TxnStatus::Idle {
                    let _ = s.execute("ROLLBACK");
                }
            }
            None => {
                let _ = nucleus_sql::hnsw::upkeep_now(&kv);
            }
        }
        if p.sim_fs().crash_count() > 0 {
            return (states, i, true);
        }
        if record {
            states.push(dump(&*kv));
        }
    }
    (states, STEPS, false)
}

fn floats(s: &mut Session, sql: &str) -> Vec<f64> {
    match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r[0].render().parse().unwrap())
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The index's answer at a wide `ef` against the exact search's.
fn searches_exactly(kv: &Arc<dyn Kv>, ctx: &str) {
    let mut s = Session::new(kv.clone());
    let q = "[20,3,2,1]";
    let exact = floats(
        &mut s,
        &format!("SELECT (v <-> '{q}') + 0 FROM items WHERE v IS NOT NULL ORDER BY 1 LIMIT 10"),
    );
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = catalog::find_index(&*txn, "items_v").unwrap().unwrap();
    let qv = nucleus_sql::vectors::value(&[20.0, 3.0, 2.0, 1.0]);
    let found: Vec<f64> = nucleus_sql::hnsw::nearest(&*txn, &t, i, &qv, 10, 1000, 1000)
        .unwrap()
        .into_iter()
        .map(|h| h.2)
        .collect();
    assert_eq!(found.len(), exact.len(), "{ctx}");
    for (f, e) in found.iter().zip(&exact) {
        assert!((f - e).abs() < 1e-9, "{ctx}: {found:?} against {exact:?}");
    }
}

fn problems(kv: &Arc<dyn Kv>) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap()
}

#[test]
fn hnsw_survives_a_crash_at_any_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("hnsw_crash_at_any_io_point", 1, |seed| {
            let dry = Arc::new(SimPlatform::new(seed));
            let (states, done, _) = workload(&dry, seed, true);
            assert_eq!(done, STEPS);
            let total = dry.sim_fs().op_count();
            let stride = (total / 300).max(1);
            let (mut crashed, mut searched) = (0, 0);
            for n in (1..=total).step_by(stride as usize) {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, done, in_step) = workload(&p, seed, false);
                // The fold's threads may make a run's operations differ a
                // little from the dry run's: past its end, no crash.
                if p.sim_fs().crash_count() == 0 {
                    continue;
                }
                crashed += 1;
                let ctx = format!("seed {seed:#x}, {mode:?} crash at op {n} (step {done})");
                let db = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let kv: Arc<dyn Kv> = Arc::new(db);
                let got = dump(&*kv);
                let done = done as usize;
                let ok = got == states[done] || (in_step && states.get(done + 1) == Some(&got));
                assert!(ok, "{ctx}: recovered a state that no step left");
                assert_eq!(problems(&kv), Vec::<String>::new(), "{ctx}");
                if !got.contains("items") || done < 3 {
                    continue; // the index not made yet
                }
                searches_exactly(&kv, &ctx);
                searched += 1;
                // And it keeps working: writes, the upkeep, a search.
                let mut s = Session::new(kv.clone());
                s.execute("INSERT INTO items VALUES (90000, '[20,3,2,1]', 0)")
                    .unwrap();
                nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
                assert_eq!(problems(&kv), Vec::<String>::new(), "{ctx}: after");
                searches_exactly(&kv, &ctx);
            }
            let runs = total.div_ceil(stride);
            eprintln!("{mode:?}: {runs} runs, {crashed} crashed, {searched} with the index");
            assert!(crashed * 10 >= runs * 9, "{crashed} of {runs} runs crashed");
            assert!(
                searched * 2 >= crashed,
                "{searched} of {crashed} with the index"
            );
        });
    }
}

#[test]
fn the_hnsw_workload_does_what_it_should() {
    // The dry run writes, seals and folds: the graph holds rows at the end.
    let p = Arc::new(SimPlatform::new(5));
    let (_, done, _) = workload(&p, 5, false);
    assert_eq!(done, STEPS);
    let db = open(&p).unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = catalog::find_index(&*txn, "items_v").unwrap().unwrap();
    let st = nucleus_sql::hnsw::status(&*txn, &t, i).unwrap();
    assert!(st.nodes >= 150, "{st:?}");
    drop(txn);
    assert_eq!(problems(&kv), Vec::<String>::new());
    searches_exactly(&kv, "dry run");
}
