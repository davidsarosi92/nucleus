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
    // keyspace 1, key encoding 2, little-endian u16s
    assert_eq!(record(&*kv), Some(vec![1, 0, 2, 0]));
}

#[test]
fn a_newer_format_is_refused() {
    let kv = db();
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    txn.put(&[0x00, 0x00], &[1, 0, 3, 0]).unwrap();
    txn.commit().unwrap();
    let err = Session::new(kv.clone()).execute("SELECT 1").unwrap_err();
    assert_eq!(err.code, "0A000");
    assert!(err.message.contains("key encoding 3"), "{err}");
    // Inside a transaction block too.
    let mut s = Session::new(kv);
    s.execute("BEGIN").unwrap();
    assert_eq!(s.execute("SELECT 1").unwrap_err().code, "0A000");
}

/// Key encoding 1 put a `NULL` array element first; 2 puts it last, as
/// comparisons do.  A version-1 database is upgraded by the first
/// transaction that may write: the keys of tables with an array in a key
/// are rewritten.
#[test]
fn an_older_key_encoding_is_upgraded() {
    use nucleus_sql::{Output, Value, catalog, encoding};
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE a (k INT[] PRIMARY KEY, n INT)")
        .unwrap();
    s.execute("CREATE INDEX a_n ON a (n)").unwrap();
    s.execute("INSERT INTO a VALUES ('{1,NULL}', 1), ('{1,2}', 2), ('{NULL}', 3), ('{}', 4)")
        .unwrap();
    // Turn it into what version 1 wrote: NULL elements as 01 00.
    let v1_key = |vals: &[Value]| -> Vec<u8> {
        let mut out = Vec::new();
        for v in vals {
            match v {
                Value::Array(items) => {
                    out.push(0x0A);
                    for item in items {
                        out.push(0x01);
                        encoding::encode_key_value(&mut out, item);
                    }
                    out.push(0x00);
                }
                v => encoding::encode_key_value(&mut out, v),
            }
        }
        out
    };
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    let t = catalog::table(&*txn, "a").unwrap();
    let prefix = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&t.data_prefix()).unwrap();
    let all: Vec<(Vec<u8>, Vec<u8>)> = txn
        .scan(
            std::ops::Bound::Included(&t.data_prefix()[..]),
            std::ops::Bound::Excluded(&end[..]),
        )
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (k, _) in &all {
        txn.delete(k).unwrap();
    }
    for (_, v) in all.iter().filter(|(k, _)| k.starts_with(&prefix)) {
        let row = encoding::decode_row(v, 2).unwrap();
        let mut key = prefix.clone();
        key.extend(v1_key(&row[..1]));
        txn.put(&key, v).unwrap();
        let mut ix = t.index_prefix(t.indexes[0].id);
        ix.extend(v1_key(&row[1..2]));
        ix.extend(v1_key(&row[..1]));
        txn.put(&ix, &v1_key(&row[..1])).unwrap();
    }
    txn.put(&[0x00, 0x00], &[1, 0, 1, 0]).unwrap();
    txn.commit().unwrap();
    // A read-only statement reads it as it is; the first write upgrades it.
    let mut s = Session::new(kv.clone());
    s.execute("SELECT count(*) FROM a").unwrap();
    assert_eq!(record(&*kv), Some(vec![1, 0, 1, 0]));
    s.execute("CREATE TABLE other (x INT)").unwrap();
    assert_eq!(record(&*kv), Some(vec![1, 0, 2, 0]));
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
    drop(txn);
    // check() found every key in the current encoding; and the rows read.
    match s.execute("SELECT n FROM a ORDER BY k").unwrap().pop() {
        Some(Output::Rows { rows, .. }) => {
            let ns: Vec<String> = rows.iter().map(|r| r[0].render()).collect();
            assert_eq!(ns, ["4", "2", "1", "3"]);
        }
        other => panic!("{other:?}"),
    }
}
