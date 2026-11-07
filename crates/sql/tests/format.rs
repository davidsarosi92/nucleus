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
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
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

/// Before integers had widths, `INT`, `INTEGER` and `SMALLINT` columns were
/// 64-bit (column tag 2, as `BIGINT`'s, in table record version 5), and
/// rows held every integer in 8 bytes (row version 1).  Such a database
/// opens with those columns as `bigint`, its values intact; `ALTER COLUMN
/// … TYPE integer` narrows one, checking its values.
#[test]
fn integer_columns_from_before_widths_read_as_bigint() {
    use nucleus_sql::{Output, catalog, encoding};
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE old (id BIGINT PRIMARY KEY, n BIGINT)")
        .unwrap();
    s.execute("INSERT INTO old VALUES (1, 5), (2, 3000000000)")
        .unwrap();
    // What the old build wrote: table record version 5, version-1 rows.
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    let t = catalog::table(&*txn, "old").unwrap();
    let key = {
        let mut k = vec![0x00, 0x01];
        k.extend_from_slice(b"old");
        k
    };
    let mut record = txn.get(&key).unwrap().unwrap();
    assert_eq!(record[0], 14, "the current table record version");
    record[0] = 5;
    txn.put(&key, &record).unwrap();
    let prefix = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    let rows: Vec<(Vec<u8>, Vec<u8>)> = txn
        .scan(
            std::ops::Bound::Included(&prefix[..]),
            std::ops::Bound::Excluded(&end[..]),
        )
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (k, v) in rows {
        let row = encoding::decode_row(&v, 2).unwrap();
        let mut old = vec![1, 2, 0];
        for v in &row {
            let nucleus_sql::Value::Int(i) = v else {
                panic!("{v:?}")
            };
            old.push(2);
            old.extend_from_slice(&i.to_le_bytes());
        }
        txn.put(&k, &old).unwrap();
    }
    txn.commit().unwrap();
    let rows = |s: &mut Session, sql: &str| match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let mut s = Session::new(kv.clone());
    assert_eq!(
        rows(&mut s, "SELECT id, n FROM old ORDER BY id"),
        ["1 5", "2 3000000000"]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT type FROM nc_catalog.columns WHERE table_name = 'old' ORDER BY position"
        ),
        ["bigint", "bigint"]
    );
    // Narrowing checks every value.
    let err = s
        .execute("ALTER TABLE old ALTER n TYPE integer")
        .unwrap_err();
    assert_eq!(err.code, "22003", "{err}");
    s.execute("DELETE FROM old WHERE id = 2").unwrap();
    s.execute("ALTER TABLE old ALTER n TYPE integer").unwrap();
    assert_eq!(rows(&mut s, "SELECT n + 1 FROM old"), ["6"]);
    assert_eq!(
        s.execute("INSERT INTO old VALUES (3, 3000000000)")
            .unwrap_err()
            .code,
        "22003"
    );
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Rows as row version 2 wrote them (no column offsets, text with `u32`
/// lengths) beside version 3's, in a table wide enough for offsets: every
/// query reads the same a row at a time and by batches, and a row
/// updated is written anew.
#[test]
fn rows_of_version_2_read_beside_version_3() {
    use nucleus_sql::{Output, Value, catalog, encoding};
    let kv = db();
    let mut s = Session::new(kv.clone());
    let cols: Vec<String> = (1..40)
        .map(|i| match i % 4 {
            0 => format!("i{i} INT"),
            1 => format!("t{i} TEXT"),
            2 => format!("b{i} BYTEA"),
            _ => format!("c{i} CHAR(3)"),
        })
        .collect();
    s.execute(&format!(
        "CREATE TABLE wide (id INT PRIMARY KEY, {})",
        cols.join(", ")
    ))
    .unwrap();
    for id in 0..40 {
        let vals: Vec<String> = (1..40)
            .map(|i| match i % 4 {
                0 => format!("{}", id * 1000 + i),
                1 => format!("'{}'", "t".repeat((id * 37 + i) % 300)),
                // Even rows hold a value long enough to be stored out of line.
                2 if id % 2 == 0 && i == 6 => format!("'\\x{}'", "ab".repeat(2500)),
                2 => format!("'\\x{:02x}'", i),
                _ => {
                    if (id + i) % 5 == 0 {
                        "NULL".into()
                    } else {
                        format!("'c{}'", i % 10)
                    }
                }
            })
            .collect();
        s.execute(&format!(
            "INSERT INTO wide VALUES ({id}, {})",
            vals.join(", ")
        ))
        .unwrap();
    }
    // Odd rows back to version 2.
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    let t = catalog::table(&*txn, "wide").unwrap();
    let prefix = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    let rows: Vec<(Vec<u8>, Vec<u8>)> = txn
        .scan(
            std::ops::Bound::Included(&prefix[..]),
            std::ops::Bound::Excluded(&end[..]),
        )
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut rewritten = 0;
    for (k, v) in rows {
        assert_eq!(v[0], 3, "rows are written in version 3");
        // (Even rows' long values, out of line, are not rewritten.)
        let row = encoding::decode_row_with(&v, 40, &mut |_, _| Ok(Value::Null)).unwrap();
        let Value::Int(id) = row[0] else { panic!() };
        if id % 2 == 0 {
            continue;
        }
        let mut old = vec![2];
        old.extend_from_slice(&40u16.to_le_bytes());
        for v in &row {
            encoding::encode_value(&mut old, v);
        }
        assert!(old.len() > v.len(), "version 3 is the shorter");
        txn.put(&k, &old).unwrap();
        rewritten += 1;
    }
    assert_eq!(rewritten, 20);
    txn.commit().unwrap();
    let render = |s: &mut Session, sql: &str| match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let queries = [
        "SELECT * FROM wide ORDER BY id",
        "SELECT id, i36, t37 FROM wide ORDER BY id",
        "SELECT id, length(b6), c39 FROM wide WHERE i20 % 3 = 1 ORDER BY id",
        "SELECT count(*), sum(i32), max(length(t33)), count(c19) FROM wide",
        "SELECT t17, count(*) FROM wide GROUP BY t17 ORDER BY t17",
    ];
    let mut want = Vec::new();
    for batch in [0, 3, 1024] {
        let mut s = Session::new(kv.clone());
        s.execute(&format!("SET batch_size = {batch}")).unwrap();
        let got: Vec<Vec<String>> = queries.iter().map(|q| render(&mut s, q)).collect();
        if want.is_empty() {
            want = got;
        } else {
            assert_eq!(got, want, "batch_size {batch}");
        }
    }
    assert_eq!(want[1][3], format!("3|3036|{}", "t".repeat(148)));
    assert_eq!(want[3], ["40|781280|292|32"]);
    // An update writes the row anew, in version 3.
    s.execute("UPDATE wide SET i4 = -1 WHERE id = 3").unwrap();
    assert_eq!(
        render(&mut s, "SELECT i4, i36, t1 FROM wide WHERE id = 3"),
        [format!("-1|3036|{}", "t".repeat(112))]
    );
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}
