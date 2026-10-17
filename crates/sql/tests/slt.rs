//! Runs every `tests/slt/*.slt` script against a fresh in-memory database
//! (PLAN.md §M9 "done when").

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::ext::Registry;
use nucleus_sql::pgcatalog::PgCompat;
use nucleus_sql::slt::{Connection, Local, run_script};

/// Runs every script in `dir` (`SLT=<name>` picks some), each against a
/// fresh database, with sessions on `registry`.
fn run_dir(dir: &str, registry: Arc<Registry>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
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
        let mut connect = || -> Box<dyn Connection> {
            Box::new(Local(Session::with_registry(
                Arc::clone(&db),
                registry.clone(),
            )))
        };
        failures.extend(run_script(&script, &name, &mut connect));
        ran += 1;
    }
    assert!(ran > 0 || only.is_some(), "no scripts found");
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn sqllogictest_scripts() {
    run_dir("tests/slt", Registry::shared_builtin());
}

/// The pg-compat module's scripts, with it registered.
#[test]
fn sqllogictest_scripts_pg_compat() {
    let mut registry = Registry::builtin();
    registry.add(&PgCompat).unwrap();
    run_dir("tests/slt-pg", Arc::new(registry));
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

/// Without pg-compat a session has no PostgreSQL catalog or functions
/// (PLAN.md principle 11); with it, it has both.
#[test]
fn pg_compat_only_where_registered() {
    let db = || -> Arc<dyn Kv> {
        Arc::new(
            Database::open_on(
                Arc::new(SimPlatform::new(1)),
                Path::new("/db"),
                Options::default(),
            )
            .unwrap(),
        )
    };
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
    let mut registry = Registry::builtin();
    registry.add(&PgCompat).unwrap();
    let mut s = Session::with_registry(db(), Arc::new(registry));
    s.execute("SELECT count(*) FROM pg_class").unwrap();
    s.execute("SELECT pg_typeof(1)").unwrap();
    assert_eq!(
        names(&mut s),
        ["information_schema", "nc_catalog", "pg_catalog", "public"]
    );
}
