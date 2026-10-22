//! Enums (PLAN.md §M14.26): labels added anywhere and renamed without
//! rewriting a row; transactional like the rest of the catalog; labels as
//! parameters.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value, catalog};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(5)),
            Path::new("/db"),
            nucleus_kv::Options::default(),
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Every key and value of table `name`: its rows and index entries.
fn stored(kv: &dyn Kv, name: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let t = catalog::table(&*txn, name).unwrap();
    let p = t.data_prefix();
    let end = nucleus_kv::prefix_end(&p).unwrap();
    txn.scan(Bound::Included(&p[..]), Bound::Excluded(&end[..]))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn new_and_renamed_labels_rewrite_no_row() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TYPE level AS ENUM ('low', 'high')")
        .unwrap();
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, l level)")
        .unwrap();
    s.execute("CREATE INDEX t_l ON t (l)").unwrap();
    s.execute("INSERT INTO t VALUES (1, 'high'), (2, 'low'), (3, 'high')")
        .unwrap();
    let before = stored(&*kv, "t");
    s.execute("ALTER TYPE level ADD VALUE 'mid' BEFORE 'high'")
        .unwrap();
    s.execute("ALTER TYPE level ADD VALUE 'lowest' BEFORE 'low'")
        .unwrap();
    s.execute("ALTER TYPE level RENAME VALUE 'high' TO 'top'")
        .unwrap();
    assert_eq!(stored(&*kv, "t"), before);
    s.execute("INSERT INTO t VALUES (4, 'mid'), (5, 'lowest')")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id, l FROM t ORDER BY l, id"),
        ["5 lowest", "2 low", "4 mid", "1 top", "3 top"]
    );
    // The index serves the new label too.
    assert_eq!(rows(&mut s, "SELECT id FROM t WHERE l = 'mid'"), ["4"]);
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn labels_are_transactional_and_parameters() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TYPE level AS ENUM ('low', 'high')")
        .unwrap();
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, l level)")
        .unwrap();
    // A label added in a rolled-back transaction is gone.
    s.execute("BEGIN").unwrap();
    s.execute("ALTER TYPE level ADD VALUE 'mid'").unwrap();
    s.execute("INSERT INTO t VALUES (1, 'mid')").unwrap();
    s.execute("ROLLBACK").unwrap();
    assert_eq!(
        s.execute("INSERT INTO t VALUES (1, 'mid')")
            .unwrap_err()
            .code,
        "22P02"
    );
    // Labels as parameters: stored, and compared.
    let insert = nucleus_sql::parse("INSERT INTO t VALUES ($1, $2)").unwrap();
    for (id, l) in [(1, "high"), (2, "low")] {
        s.run(&insert[0], &[Value::Int(id), Value::Text(l.into())])
            .unwrap();
    }
    let select = nucleus_sql::parse("SELECT id FROM t WHERE l = $1").unwrap();
    match s.run(&select[0], &[Value::Text("low".into())]).unwrap() {
        Output::Rows { rows, .. } => assert_eq!(rows, [[Value::Int(2)]]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        s.run(&select[0], &[Value::Text("nope".into())])
            .unwrap_err()
            .code,
        "22P02"
    );
}
