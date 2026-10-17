//! The module's SQL side in a local session: its scripts, and that a
//! registry with it has PostgreSQL's catalog and functions.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_pg_compat::PgCompat;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::{Output, Registry, Session};
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

fn registry() -> Arc<Registry> {
    let mut r = Registry::builtin();
    r.add(&PgCompat).unwrap();
    Arc::new(r)
}

#[test]
fn scripts_in_a_local_session() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    let mut failures = Vec::new();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let script = std::fs::read_to_string(f).unwrap();
        let db = db();
        let mut connect = || -> Box<dyn Connection> {
            Box::new(Local(Session::with_registry(Arc::clone(&db), registry())))
        };
        failures.extend(run_script(&script, &name, &mut connect));
    }
    assert!(!files.is_empty());
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn a_registry_with_it_has_postgresql_s_catalog() {
    let mut s = Session::with_registry(db(), registry());
    s.execute("SELECT count(*) FROM pg_class").unwrap();
    s.execute("SELECT pg_typeof(1)").unwrap();
    let names = match s
        .execute("SELECT name FROM nc_catalog.schemas ORDER BY name")
        .unwrap()
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows.iter().map(|r| r[0].render()).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        names,
        ["information_schema", "nc_catalog", "pg_catalog", "public"]
    );
}
