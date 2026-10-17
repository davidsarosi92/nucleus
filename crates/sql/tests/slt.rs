//! Runs every `tests/slt/*.slt` script against a fresh in-memory database
//! (PLAN.md §M9 "done when").

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::slt::{Connection, Local, run_script};

#[test]
fn sqllogictest_scripts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    let only = std::env::var("SLT").ok();
    let mut failures = Vec::new();
    let mut ran = 0;
    for f in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "slt"))
    {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        if only.as_ref().is_some_and(|o| !name.contains(o.as_str())) {
            continue;
        }
        let db: Arc<dyn Kv> = Arc::new(
            Database::open_on(
                Arc::new(SimPlatform::new(1)),
                Path::new("/db"),
                Options::default(),
            )
            .unwrap(),
        );
        let script = std::fs::read_to_string(f).unwrap();
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Local(Session::new(Arc::clone(&db)))) };
        failures.extend(run_script(&script, &name, &mut connect));
        ran += 1;
    }
    assert!(ran > 0, "no scripts found");
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// `now()` reads the database's clock — under the simulation, the simulated
/// one (PLAN.md principle 4) — and stays fixed within a transaction.
#[test]
fn now_is_the_simulated_clock() {
    let sim = Arc::new(SimPlatform::new(1));
    let db: Arc<dyn Kv> =
        Arc::new(Database::open_on(sim.clone(), Path::new("/db"), Options::default()).unwrap());
    let mut s = Session::new(db);
    let one = |s: &mut Session, sql: &str| match s.execute(sql).unwrap().pop() {
        Some(nucleus_sql::Output::Rows { rows, .. }) => rows[0][0].render(),
        other => panic!("{other:?}"),
    };
    assert_eq!(one(&mut s, "SELECT now()::text"), "2026-01-01 00:00:00+00");
    s.execute("BEGIN").unwrap();
    sim.sim_clock().advance(std::time::Duration::from_secs(90));
    assert_eq!(one(&mut s, "SELECT now()::text"), "2026-01-01 00:01:30+00");
    sim.sim_clock().advance(std::time::Duration::from_secs(30));
    assert_eq!(one(&mut s, "SELECT now()::text"), "2026-01-01 00:01:30+00");
    assert_eq!(
        one(&mut s, "SELECT clock_timestamp()::text"),
        "2026-01-01 00:02:00+00"
    );
    s.execute("COMMIT").unwrap();
}
