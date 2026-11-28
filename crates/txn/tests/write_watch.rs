//! Watches of writes ([`Engine::watch_writes`]; PLAN.md §M16.6 S1): a
//! commit that writes into a watched range raises the watch's mark past
//! every snapshot that did not see it, and wakes its waiters — what a
//! live query re-runs on.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Fill, Timestamp, Transaction};

fn engine(opts: EngineOptions) -> Engine {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(1));
    Engine::open(p, Path::new("/db"), opts).unwrap()
}

fn put(e: &Engine, k: &[u8]) {
    let mut t = e.begin();
    t.put(k, b"v").unwrap();
    t.commit().unwrap();
}

fn range(s: &[u8], e: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![(s.to_vec(), e.to_vec())]
}

/// The timestamp of a snapshot taken now.
fn now(e: &Engine) -> Timestamp {
    e.begin().read_ts()
}

#[test]
fn a_commit_into_a_watched_range_moves_its_mark_past_the_snapshot() {
    let e = engine(EngineOptions::default());
    put(&e, b"a");
    let w = e.watch_writes(range(b"c", b"d"));
    // At first, the newest commit: a snapshot taken now sees it.
    let at = now(&e);
    assert_eq!(w.last(), at);
    assert_eq!(w.wait(at, Duration::ZERO), at);
    // Writes outside the range, and a rollback inside it, leave it.
    put(&e, b"b");
    put(&e, b"d");
    let mut t = e.begin();
    t.put(b"c1", b"v").unwrap();
    t.rollback();
    assert_eq!(w.last(), at);
    // A commit inside: the mark is its timestamp, past the snapshot.
    put(&e, b"c");
    let c = now(&e);
    assert_eq!(w.last(), c);
    assert!(c > at);
    assert_eq!(w.wait(at, Duration::ZERO), c);
    // A delete is a write.
    let mut t = e.begin();
    t.delete(b"c").unwrap();
    t.commit().unwrap();
    assert!(w.last() > c);
    // Another watch on another range; dropping one leaves the other.
    let other = e.watch_writes(range(b"x", b"y"));
    assert_eq!(e.write_watches(), 2);
    drop(w);
    assert_eq!(e.write_watches(), 1);
    put(&e, b"x1");
    assert_eq!(other.last(), now(&e));
    drop(other);
    assert_eq!(e.write_watches(), 0);
}

#[test]
fn several_ranges_and_watches_each_see_their_own() {
    let e = engine(EngineOptions::default());
    let w = e.watch_writes(vec![
        (b"a".to_vec(), b"b".to_vec()),
        (b"m".to_vec(), b"n".to_vec()),
    ]);
    let v = e.watch_writes(range(b"m", b"z"));
    let at = now(&e);
    put(&e, b"q");
    assert_eq!(w.last(), at);
    assert_eq!(v.last(), now(&e));
    put(&e, b"a5");
    assert_eq!(w.last(), now(&e));
    // The end is excluded, the start included.
    let before = now(&e);
    put(&e, b"b");
    put(&e, b"z");
    assert_eq!(w.last(), before);
    put(&e, b"m");
    assert_eq!(w.last(), now(&e));
    assert_eq!(v.last(), now(&e));
}

#[test]
fn appends_to_a_stream_in_the_range_move_the_mark() {
    let e = engine(EngineOptions::default());
    let entries = e.watch_writes(range(b"s/e", b"s/f"));
    let head = e.watch_writes(range(b"s/h", b"s/i"));
    let at = now(&e);
    let mut t: Transaction = e.begin();
    let fill: Fill = |v, n, time| {
        v[..8].copy_from_slice(&n.to_be_bytes());
        v[8..16].copy_from_slice(&time.to_be_bytes());
    };
    t.append(b"s/h", b"s/e", &[0u8; 20], fill).unwrap();
    t.commit().unwrap();
    assert!(entries.last() > at);
    assert!(head.last() > at);
}

#[test]
fn a_spilled_transaction_and_a_fresh_range_move_the_mark() {
    let e = engine(EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            ..Default::default()
        },
        txn_memory: 64 << 10,
        wal_minimal: true,
        ..EngineOptions::default()
    });
    let w = e.watch_writes(range(b"big", b"bih"));
    let f = e.watch_writes(range(b"new/", b"new0"));
    // Spilled: more than the transaction's memory.
    let at = now(&e);
    let mut t = e.begin();
    for i in 0..4000u32 {
        t.put(format!("big{i:06}").as_bytes(), &[7; 64]).unwrap();
    }
    t.commit().unwrap();
    assert!(w.last() > at);
    assert_eq!(f.last(), at);
    // A fresh range, written in key order without the WAL.
    let at = now(&e);
    let mut t = e.begin();
    t.fresh(b"new/", b"new0");
    for i in 0..1000u32 {
        t.put(format!("new/{i:06}").as_bytes(), b"v").unwrap();
    }
    t.commit().unwrap();
    assert!(f.last() > at);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_waiter_wakes_at_a_commit_into_its_range_and_not_before() {
    let e = engine(EngineOptions::default());
    let w = e.watch_writes(range(b"c", b"d"));
    let at = now(&e);
    // Nothing written in it: the wait runs out.
    let t0 = std::time::Instant::now();
    assert_eq!(w.wait(at, Duration::from_millis(30)), at);
    assert!(t0.elapsed() >= Duration::from_millis(30));
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            put(&e, b"b"); // outside
            std::thread::sleep(Duration::from_millis(30));
            put(&e, b"c");
        });
        let t0 = std::time::Instant::now();
        let got = w.wait(at, Duration::from_secs(30));
        assert!(got > at);
        assert!(t0.elapsed() >= Duration::from_millis(60));
        assert!(t0.elapsed() < Duration::from_secs(30));
    });
}

/// A reader that re-reads a range each time its watch says something it
/// did not see was written: under writers it never misses a commit — its
/// last read, once the writers are done, is the range as it is.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_reader_led_by_its_watch_misses_no_commit() {
    let e = engine(EngineOptions::default());
    let (writers, each) = (4, 200);
    let count = |t: &Transaction| {
        let mut n = 0;
        for kv in t
            .scan(Bound::Included(b"k"), Bound::Excluded(b"l"))
            .unwrap()
        {
            kv.unwrap();
            n += 1;
        }
        n
    };
    let w = e.watch_writes(range(b"k", b"l"));
    std::thread::scope(|s| {
        for i in 0..writers {
            let e = &e;
            s.spawn(move || {
                for j in 0..each {
                    put(e, format!("k{i}-{j:04}").as_bytes());
                    put(e, format!("j{i}-{j:04}").as_bytes()); // outside
                }
            });
        }
        let mut reads = 0;
        loop {
            let t = e.begin();
            let (at, n) = (t.read_ts(), count(&t));
            drop(t);
            reads += 1;
            if n == writers * each {
                break;
            }
            // Not all there yet: a commit the read did not see is coming
            // (or came) and raises the mark past it.
            let got = w.wait(at, Duration::from_secs(60));
            assert!(got > at, "a commit was missed: read {n} at {at}");
        }
        assert!(reads >= 2);
    });
}
