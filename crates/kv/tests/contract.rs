//! The KV contract's behaviour, as a suite any implementation must pass.
//! Today it runs against the local `Database`; the distributed one (§F3)
//! will run the same suite.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Error, Kv, KvTxn, Options, TxnOptions, prefix_end, transact};
use nucleus_platform::sim::SimPlatform;

fn put(kv: &dyn Kv, k: &str, v: &str) {
    transact(kv, TxnOptions::default(), |t| {
        t.put(k.as_bytes(), v.as_bytes())
    })
    .unwrap();
}

fn get(t: &dyn KvTxn, k: &str) -> Option<String> {
    t.get(k.as_bytes())
        .unwrap()
        .map(|v| String::from_utf8(v).unwrap())
}

fn keys(t: &dyn KvTxn, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Vec<String> {
    t.scan(start, end)
        .unwrap()
        .map(|r| String::from_utf8(r.unwrap().0).unwrap())
        .collect()
}

/// The contract.
fn contract(kv: &dyn Kv) {
    // Basic reads and writes, and atomic commit.
    let mut t = kv.begin(TxnOptions::default()).unwrap();
    for (k, v) in [("b", "2"), ("a", "1"), ("c", "3"), ("ca", "4"), ("d", "5")] {
        t.put(k.as_bytes(), v.as_bytes()).unwrap();
    }
    assert_eq!(get(t.as_ref(), "a").as_deref(), Some("1")); // own writes
    let other = kv.begin(TxnOptions::default()).unwrap();
    assert_eq!(get(other.as_ref(), "a"), None); // not yet committed
    let ts = t.commit().unwrap();
    assert!(ts > other.read_ts());
    drop(other);

    // Scans: bounds, prefixes, order.
    let t = kv.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        keys(t.as_ref(), Bound::Unbounded, Bound::Unbounded),
        ["a", "b", "c", "ca", "d"]
    );
    assert_eq!(
        keys(t.as_ref(), Bound::Excluded(b"a"), Bound::Included(b"c")),
        ["b", "c"]
    );
    let end = prefix_end(b"c").unwrap();
    assert_eq!(
        keys(t.as_ref(), Bound::Included(b"c"), Bound::Excluded(&end)),
        ["c", "ca"]
    );
    // A scan may be open while the same transaction reads keys.
    let mut scan = t.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
    let (first, _) = scan.next().unwrap().unwrap();
    assert_eq!(get(t.as_ref(), "d").as_deref(), Some("5"));
    assert_eq!(first, b"a");
    drop(scan);
    drop(t);

    // Read-only transactions refuse writes.
    let mut ro = kv.begin(TxnOptions::read_only()).unwrap();
    assert!(matches!(ro.put(b"x", b"y"), Err(Error::ReadOnly)));

    // Deletes, and rollback.
    let mut t = kv.begin(TxnOptions::default()).unwrap();
    t.delete(b"b").unwrap();
    t.delete(b"nonexistent").unwrap();
    t.commit().unwrap();
    let mut t = kv.begin(TxnOptions::default()).unwrap();
    t.put(b"rolled", b"back").unwrap();
    t.rollback();
    let t = kv.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(
        keys(t.as_ref(), Bound::Unbounded, Bound::Unbounded),
        ["a", "c", "ca", "d"]
    );
    drop(t);

    // Snapshots: an old one keeps seeing old data.
    let old = kv.begin(TxnOptions::read_only()).unwrap();
    put(kv, "a", "changed");
    assert_eq!(get(old.as_ref(), "a").as_deref(), Some("1"));
    let at = kv
        .begin(TxnOptions {
            at: Some(old.read_ts()),
            ..TxnOptions::default()
        })
        .unwrap();
    assert_eq!(get(at.as_ref(), "a").as_deref(), Some("1"));
    drop((old, at));

    // Conflicts are reported as retryable, and `transact` retries them.
    let mut t1 = kv.begin(TxnOptions::default()).unwrap();
    let mut t2 = kv.begin(TxnOptions::default()).unwrap();
    t1.put(b"x", b"1").unwrap();
    t2.put(b"x", b"2").unwrap();
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(err.is_retryable(), "{err}");
    let mut tries = 0;
    let n: u64 = transact(kv, TxnOptions::default(), |t| {
        tries += 1;
        if tries == 1 {
            put(kv, "x", "interference"); // a concurrent commit
        }
        let v = t.get(b"x")?.unwrap();
        t.put(b"x", b"done")?;
        Ok(v.len() as u64)
    })
    .unwrap();
    assert_eq!((tries, n), (2, "interference".len() as u64));

    // Binary keys and values, empty ones included.
    let mut t = kv.begin(TxnOptions::default()).unwrap();
    t.put(b"", b"").unwrap();
    t.put(b"\x00\xff\x00", b"\x00").unwrap();
    t.commit().unwrap();
    let t = kv.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(t.get(b"").unwrap(), Some(vec![]));
    assert_eq!(t.get(b"\x00\xff\x00").unwrap(), Some(vec![0]));
    assert!(matches!(
        kv.begin(TxnOptions::default())
            .unwrap()
            .put(&vec![1; 100_000], b""),
        Err(Error::KeyTooLarge(_))
    ));
}

#[test]
fn local_database_on_the_simulation() {
    let p = Arc::new(SimPlatform::new(1));
    let db = Database::open_on(
        p,
        Path::new("/db"),
        Options {
            history: 10,
            ..Options::default()
        },
    )
    .unwrap();
    contract(&db);
}

#[test]
fn local_database_on_the_real_file_system() {
    let dir = std::env::temp_dir().join(format!("nucleus-kv-{}", std::process::id()));
    let db = Database::open(
        &dir,
        Options {
            history: 10,
            ..Options::default()
        },
    )
    .unwrap();
    contract(&db);
    db.close().unwrap();
    // And it reopens with the data.
    let db = Database::open(&dir, Options::default()).unwrap();
    let t = db.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(get(t.as_ref(), "x").as_deref(), Some("done"));
    drop(t);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn many_threads_through_the_trait() {
    let p = Arc::new(SimPlatform::new(2));
    let db: Arc<dyn Kv> =
        Arc::new(Database::open_on(p, Path::new("/db"), Options::default()).unwrap());
    std::thread::scope(|s| {
        for t in 0..8 {
            let db = Arc::clone(&db);
            s.spawn(move || {
                for _ in 0..20 {
                    transact(db.as_ref(), TxnOptions::default(), |txn| {
                        let n = txn
                            .get(b"n")?
                            .map_or(0, |v| u64::from_le_bytes(v.try_into().unwrap()));
                        txn.put(b"n", &(n + 1).to_le_bytes())?;
                        txn.put(format!("t{t}").as_bytes(), b"seen")
                    })
                    .unwrap();
                }
            });
        }
    });
    let t = db.begin(TxnOptions::read_only()).unwrap();
    assert_eq!(t.get(b"n").unwrap(), Some(160u64.to_le_bytes().to_vec()));
}

#[test]
fn one_process_at_a_time_per_directory() {
    let dir = std::env::temp_dir().join(format!("nucleus-kv-lock-{}", std::process::id()));
    let db = Database::open(&dir, Options::default()).unwrap();
    let err = match Database::open(&dir, Options::default()) {
        Ok(_) => panic!("a second open succeeded"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("in use by another process"), "{err}");
    db.close().unwrap();
    // Closed: it opens again.
    let db = Database::open(&dir, Options::default()).unwrap();
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
