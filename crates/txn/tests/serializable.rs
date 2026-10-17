//! Serializable isolation (PLAN.md §M14): read-set validation.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_txn::{Engine, EngineOptions, Error, Isolation, Transaction, TxnOptions};

fn engine() -> Engine {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(1));
    Engine::open(p, Path::new("/db"), EngineOptions::default()).unwrap()
}

fn ser(e: &Engine) -> Transaction {
    e.begin_with(TxnOptions {
        isolation: Isolation::Serializable,
        ..TxnOptions::default()
    })
    .unwrap()
}

fn put(e: &Engine, k: &[u8], v: &[u8]) {
    let mut t = e.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

#[test]
fn write_skew_is_prevented() {
    let e = engine();
    put(&e, b"alice", b"on");
    put(&e, b"bob", b"on");
    let mut t1 = ser(&e);
    let mut t2 = ser(&e);
    assert_eq!(t1.get(b"bob").unwrap().as_deref(), Some(&b"on"[..]));
    t1.put(b"alice", b"off").unwrap();
    assert_eq!(t2.get(b"alice").unwrap().as_deref(), Some(&b"on"[..]));
    t2.put(b"bob", b"off").unwrap();
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(
        matches!(err, Error::Conflict { ref key } if key == b"alice"),
        "{err}"
    );
    let t = e.begin();
    assert_eq!(
        t.get(b"bob").unwrap().as_deref(),
        Some(&b"on"[..]),
        "one doctor stays on call"
    );
}

#[test]
fn phantoms_are_prevented() {
    let e = engine();
    put(&e, b"task/1", b"x");
    let mut t1 = ser(&e);
    // "If fewer than 2 tasks exist, add one" — in two transactions at once.
    let n = t1
        .scan(Bound::Included(b"task/"), Bound::Excluded(b"task0"))
        .unwrap()
        .count();
    assert_eq!(n, 1);
    let mut t2 = ser(&e);
    assert_eq!(
        t2.scan(Bound::Included(b"task/"), Bound::Excluded(b"task0"))
            .unwrap()
            .count(),
        1
    );
    t2.put(b"task/2", b"y").unwrap();
    t2.commit().unwrap();
    t1.put(b"task/3", b"z").unwrap();
    assert!(matches!(t1.commit(), Err(Error::Conflict { .. })));
}

#[test]
fn snapshot_isolation_still_allows_write_skew() {
    let e = engine();
    put(&e, b"alice", b"on");
    put(&e, b"bob", b"on");
    let mut t1 = e.begin();
    let mut t2 = e.begin();
    t1.get(b"bob").unwrap();
    t1.put(b"alice", b"off").unwrap();
    t2.get(b"alice").unwrap();
    t2.put(b"bob", b"off").unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
}

#[test]
fn read_only_and_disjoint_transactions_commit() {
    let e = engine();
    put(&e, b"a", b"1");
    put(&e, b"b", b"1");
    let reader = ser(&e);
    let mut w1 = ser(&e);
    let mut w2 = ser(&e);
    assert!(reader.get(b"a").unwrap().is_some());
    w1.get(b"a").unwrap();
    w1.put(b"a", b"2").unwrap();
    w2.get(b"b").unwrap();
    w2.put(b"b", b"2").unwrap();
    w1.commit().unwrap();
    w2.commit().unwrap(); // read b, nobody wrote b
    reader.commit().unwrap(); // reads a snapshot: always serializable
}

#[test]
fn only_the_part_of_a_scan_that_was_read_counts() {
    let e = engine();
    for k in [b"k1", b"k2", b"k8", b"k9"] {
        put(&e, k, b"v");
    }
    let mut t = ser(&e);
    {
        let mut scan = t
            .scan(Bound::Included(b"k"), Bound::Excluded(b"l"))
            .unwrap();
        assert_eq!(scan.next().unwrap().unwrap().0, b"k1"); // stops after one row (a LIMIT 1)
    }
    put(&e, b"k5", b"inserted later"); // past what was read
    t.put(b"out", b"x").unwrap();
    t.commit().unwrap();

    let mut t = ser(&e);
    assert_eq!(
        t.scan(Bound::Included(b"k"), Bound::Excluded(b"l"))
            .unwrap()
            .count(),
        5
    );
    put(&e, b"k7", b"inside the range read");
    t.put(b"out2", b"x").unwrap();
    assert!(t.commit().is_err());
}

#[test]
fn serializable_counters_under_contention() {
    // The total of two counters may be increased only while it is < 100:
    // under snapshot isolation concurrent increments of different counters
    // overshoot; serializable keeps the invariant.
    let e = engine();
    put(&e, b"c1", &0u64.to_le_bytes());
    put(&e, b"c2", &0u64.to_le_bytes());
    let read = |t: &Transaction, k: &[u8]| {
        u64::from_le_bytes(t.get(k).unwrap().unwrap().try_into().unwrap())
    };
    std::thread::scope(|s| {
        for id in 0..6u64 {
            let e = e.clone();
            s.spawn(move || {
                let rng = SplitMix64::new(id);
                for _ in 0..40 {
                    loop {
                        let mut t = ser(&e);
                        let (a, b) = (read(&t, b"c1"), read(&t, b"c2"));
                        if a + b >= 100 {
                            break;
                        }
                        // Widen the window between the read and the write.
                        std::thread::sleep(std::time::Duration::from_micros(50));
                        let (k, v) = if rng.below(2) == 0 {
                            (&b"c1"[..], a)
                        } else {
                            (&b"c2"[..], b)
                        };
                        match t
                            .put(k, &(v + 1).to_le_bytes())
                            .and_then(|()| t.commit().map(|_| ()))
                        {
                            Ok(()) => break,
                            Err(err) if err.is_retryable() => continue,
                            Err(err) => panic!("{err}"),
                        }
                    }
                }
            });
        }
    });
    let t = e.begin();
    assert_eq!(read(&t, b"c1") + read(&t, b"c2"), 100, "the invariant held");
}
