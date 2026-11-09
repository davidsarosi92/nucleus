//! Text indexes under crashes and concurrent sessions (PLAN.md principle
//! 5, §M14.21 T4).  Crashes: writes to a table with a text index —
//! inserts, texts changed or set `NULL`, deletes, rollbacks — and the
//! index's upkeep (seals, merges) on the simulation, a crash at an I/O
//! operation, then reopen: the rows must be those after some step, the
//! integrity check must find the index exact (its rows and statistics the
//! table's), its answers must be the rows', and it must go on working.
//! Sessions: a transaction's snapshot keeps its answers and statistics
//! while other sessions write, seal and merge.

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

const WORDS: [&str; 9] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "running", "runs", "zeta", "eta",
];

fn text(rng: &SplitMix64) -> String {
    let n = 1 + rng.below(6);
    let w: Vec<&str> = (0..n)
        .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
        .collect();
    format!("'{}'", w.join(" "))
}

/// Step `i`: SQL, or `None` for the upkeep (and a merge of everything,
/// every other time) — decided by the seed alone.
fn step(rng: &SplitMix64, i: u64) -> Option<String> {
    let setup = [
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        "INSERT INTO docs SELECT g, (ARRAY['alpha','beta','gamma','delta','running'])[1 + g % 5] \
         || ' ' || (ARRAY['zeta','eta','runs'])[1 + g % 3] FROM generate_series(1, 150) g",
        "CREATE INDEX docs_body ON docs USING text (body) WITH (analyzer = 'english')",
    ];
    if let Some(s) = setup.get(i as usize) {
        return Some(s.to_string());
    }
    let k = 1 + rng.below(200);
    Some(match rng.below(8) {
        0 | 1 => {
            let rows: Vec<String> = (0..=rng.below(6))
                .map(|j| format!("({}, {})", 1000 + k * 10 + j, text(rng)))
                .collect();
            format!(
                "INSERT INTO docs VALUES {} ON CONFLICT DO NOTHING",
                rows.join(", ")
            )
        }
        2 => format!(
            "UPDATE docs SET body = {} WHERE id % 17 = {}",
            text(rng),
            k % 17
        ),
        3 => format!("DELETE FROM docs WHERE id % 19 = {}", k % 19),
        4 => format!("UPDATE docs SET body = NULL WHERE id = {k}"),
        5 => format!(
            "BEGIN; INSERT INTO docs VALUES ({}, {}); ROLLBACK",
            5000 + k,
            text(rng)
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

fn upkeep(kv: &Arc<dyn Kv>, i: u64) -> Result<(), String> {
    let e = |e: &dyn std::fmt::Debug| format!("{e:?}");
    nucleus_sql::text::index::upkeep_now(kv).map_err(|x| e(&x))?;
    if i.is_multiple_of(2) {
        let txn = kv.begin(TxnOptions::read_only()).map_err(|x| e(&x))?;
        let found = catalog::find_index(&*txn, "docs_body").map_err(|x| e(&x))?;
        drop(txn);
        if let Some((t, ix)) = found {
            nucleus_sql::text::index::merge_all(kv, &t, ix).map_err(|x| e(&x))?;
        }
    }
    Ok(())
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
                let _ = upkeep(&kv, i);
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

fn ids(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows.iter().map(|r| r[0].render()).collect(),
        other => panic!("{other:?}"),
    }
}

const QUERIES: [&str; 6] = [
    "run",
    "alpha beta",
    "gamma AND runs",
    "\"delta running\"",
    "-zeta",
    "eps*",
];

/// Each query's rows through the index and on the rows (the query from a
/// joined row, which the index cannot serve; the index's analyzer still),
/// alike.
fn answers_as_the_rows(s: &mut Session, ctx: &str) {
    for q in QUERIES {
        let by_index = ids(
            s,
            &format!("SELECT id FROM docs WHERE body @@ '{q}' ORDER BY id"),
        );
        let by_rows = ids(
            s,
            &format!(
                "SELECT d.id FROM docs d, (VALUES ('{q}')) q(q) WHERE d.body @@ q.q ORDER BY 1"
            ),
        );
        assert_eq!(by_index, by_rows, "{ctx}: {q}");
    }
}

fn problems(kv: &Arc<dyn Kv>) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap()
}

#[test]
fn a_text_index_survives_a_crash_at_any_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("text_index_crash_at_any_io_point", 1, |seed| {
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
                if !got.contains("docs") || done < 3 {
                    continue; // the index not made yet
                }
                let mut s = Session::new(kv.clone());
                answers_as_the_rows(&mut s, &ctx);
                searched += 1;
                // And it keeps working: writes, the upkeep, a search.
                s.execute("INSERT INTO docs VALUES (90000, 'alpha running')")
                    .unwrap();
                s.execute("DELETE FROM docs WHERE id % 7 = 1").unwrap();
                upkeep(&kv, 0).unwrap();
                assert_eq!(problems(&kv), Vec::<String>::new(), "{ctx}: after");
                answers_as_the_rows(&mut s, &ctx);
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
fn the_text_workload_does_what_it_should() {
    let p = Arc::new(SimPlatform::new(5));
    let (_, done, _) = workload(&p, 5, false);
    assert_eq!(done, STEPS);
    let db = open(&p).unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    assert_eq!(problems(&kv), Vec::<String>::new());
    let mut s = Session::new(kv.clone());
    answers_as_the_rows(&mut s, "dry run");
    assert!(!ids(&mut s, "SELECT id FROM docs WHERE body @@ 'run'").is_empty());
}

/// A snapshot keeps its answers and statistics while other sessions write,
/// seal and merge.
#[test]
fn a_snapshot_keeps_its_answers_through_others_writes_and_upkeep() {
    let p = Arc::new(SimPlatform::new(9));
    let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
    let rng = SplitMix64::new(9);
    let mut w = Session::new(kv.clone());
    for i in 0..3 {
        w.execute(&step(&rng, i).unwrap()).unwrap();
    }
    let mut reader = Session::new(kv.clone());
    reader
        .execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    let seen: Vec<Vec<String>> = QUERIES
        .iter()
        .map(|q| {
            ids(
                &mut reader,
                &format!("SELECT id FROM docs WHERE body @@ '{q}' ORDER BY id"),
            )
        })
        .collect();
    let snapshot = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, ix) = catalog::find_index(&*snapshot, "docs_body")
        .unwrap()
        .unwrap();
    let terms = ["run", "alpha", "zeta"];
    let stats = nucleus_sql::text::index::stats(&*snapshot, &t, ix, &terms).unwrap();
    // A writer in a transaction of its own, open across the upkeep.
    let mut other = Session::new(kv.clone());
    other.execute("BEGIN").unwrap();
    other
        .execute("INSERT INTO docs VALUES (7000, 'alpha run run')")
        .unwrap();
    for i in 3..60 {
        if let Some(sql) = step(&rng, i) {
            if w.execute(&sql).is_err() && w.status() != TxnStatus::Idle {
                w.execute("ROLLBACK").unwrap();
            }
        } else {
            upkeep(&kv, i).unwrap();
        }
        if i.is_multiple_of(10) {
            for (q, want) in QUERIES.iter().zip(&seen) {
                let got = ids(
                    &mut reader,
                    &format!("SELECT id FROM docs WHERE body @@ '{q}' ORDER BY id"),
                );
                assert_eq!(&got, want, "step {i}: {q}");
            }
            assert_eq!(
                nucleus_sql::text::index::stats(&*snapshot, &t, ix, &terms).unwrap(),
                stats,
                "step {i}"
            );
        }
    }
    other.execute("COMMIT").unwrap();
    reader.execute("COMMIT").unwrap();
    upkeep(&kv, 0).unwrap();
    assert_eq!(problems(&kv), Vec::<String>::new());
    let mut s = Session::new(kv.clone());
    answers_as_the_rows(&mut s, "after");
    assert!(ids(&mut s, "SELECT id FROM docs WHERE body @@ 'run'").contains(&"7000".to_string()));
}
