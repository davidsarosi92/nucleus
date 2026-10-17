//! The SQL layer's format record (PLAN.md principle 6): written on first
//! use, and a database in a newer format is refused, not misread.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn record(kv: &dyn Kv) -> Option<Vec<u8>> {
    kv.begin(TxnOptions::read_only())
        .unwrap()
        .get(&[0x00, 0x00])
        .unwrap()
}

#[test]
fn first_use_writes_the_record() {
    let kv = db();
    assert_eq!(record(&*kv), None);
    let mut s = Session::new(kv.clone());
    // A read-only statement leaves an unused database as it is …
    s.execute("SELECT 1").unwrap();
    assert_eq!(record(&*kv), None);
    // … the first write records the format.
    s.execute("CREATE TABLE t (a INT)").unwrap();
    // keyspace 1, key encoding 1, little-endian u16s
    assert_eq!(record(&*kv), Some(vec![1, 0, 1, 0]));
}

#[test]
fn a_newer_format_is_refused() {
    let kv = db();
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    txn.put(&[0x00, 0x00], &[1, 0, 2, 0]).unwrap();
    txn.commit().unwrap();
    let err = Session::new(kv.clone()).execute("SELECT 1").unwrap_err();
    assert_eq!(err.code, "0A000");
    assert!(err.message.contains("key encoding 2"), "{err}");
    // Inside a transaction block too.
    let mut s = Session::new(kv);
    s.execute("BEGIN").unwrap();
    assert_eq!(s.execute("SELECT 1").unwrap_err().code, "0A000");
}
