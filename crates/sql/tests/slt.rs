//! Runs the test kit's scripts (`nucleus_testkit::core_scripts`) against a
//! fresh database each (PLAN.md §M9 "done when").

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_testkit::slt::{Connection, Local, run_script};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

/// The test kit's scripts, each against a fresh database (`SLT=<name>`
/// picks some).
#[test]
fn sqllogictest_scripts() {
    let mut failures = Vec::new();
    let mut ran = 0;
    for (name, script) in nucleus_testkit::core_scripts() {
        if !nucleus_testkit::picked(name) {
            continue;
        }
        let db = db();
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Local(Session::new(Arc::clone(&db)))) };
        failures.extend(run_script(script, name, &mut connect));
        ran += 1;
    }
    assert!(ran > 0 || std::env::var("SLT").is_ok(), "no scripts found");
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

/// Without pg-compat (a module, `modules/pg-compat`) a session has no
/// PostgreSQL catalog or functions (PLAN.md principle 11).
#[test]
fn no_postgresql_without_pg_compat() {
    let mut s = Session::new(db());
    for (sql, code) in [
        ("SELECT * FROM pg_catalog.pg_class", "42P01"),
        ("SELECT * FROM pg_class", "42P01"),
        ("SELECT * FROM information_schema.tables", "42P01"),
        ("SELECT pg_typeof(1)", "42883"),
        ("SELECT pg_backend_pid()", "42883"),
        ("SELECT 'x'::regclass", "42704"),
    ] {
        assert_eq!(s.execute(sql).unwrap_err().code, code, "{sql}");
    }
    let names = |s: &mut Session| match s
        .execute("SELECT name FROM nc_catalog.schemas ORDER BY name")
        .unwrap()
        .pop()
    {
        Some(nucleus_sql::Output::Rows { rows, .. }) => {
            rows.iter().map(|r| r[0].render()).collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(names(&mut s), ["nc_catalog", "public"]);
}
