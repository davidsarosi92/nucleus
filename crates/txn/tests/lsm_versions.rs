//! An LSM tree's compactions drop the versions no snapshot sees (PLAN.md
//! §M14.28 L4d), as commits drop them from the B+tree leaves they
//! rewrite: a key written over and over keeps a few versions, not all —
//! and a snapshot held keeps the version it reads.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_txn::{Engine, EngineKind, EngineOptions};

/// The store's entries of user key `k…` (its versions).
fn stored(e: &Engine, prefix: &[u8]) -> usize {
    e.store()
        .read()
        .unwrap()
        .range(Bound::Included(prefix), Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .take_while(|(k, _)| k.starts_with(prefix))
        .count()
}

fn write(e: &Engine, k: &[u8], v: &[u8]) {
    let mut t = e.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

#[test]
fn compactions_drop_versions_no_snapshot_sees() {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(1));
    let e = Engine::open(p, Path::new("/db"), EngineOptions::default()).unwrap();
    let t = e.begin();
    assert!(t.add_engine(b"k", b"l", EngineKind::Lsm).unwrap());
    t.commit().unwrap();
    let mut old = None;
    for i in 1..=40u32 {
        write(&e, b"k1", format!("v{i}").as_bytes());
        write(&e, b"k2", b"steady");
        if i == 10 {
            old = Some(e.begin());
        }
        if i % 2 == 0 {
            e.checkpoint().unwrap();
            e.store().compact(&mut || true).unwrap();
        }
    }
    e.checkpoint().unwrap();
    e.store().compact(&mut || true).unwrap();
    let old = old.unwrap();
    assert_eq!(old.get(b"k1").unwrap().as_deref(), Some(&b"v10"[..]));
    assert_eq!(e.begin().get(b"k1").unwrap().as_deref(), Some(&b"v40"[..]));
    // v10 (what the snapshot reads) and the 30 after it stay; of the 9
    // before, what merges reached went.
    let held = stored(&e, b"k1");
    assert!(
        (31..40).contains(&held),
        "{held} versions of k1 stored with a snapshot held"
    );
    drop(old);
    for i in 41..=48u32 {
        write(&e, b"k1", format!("v{i}").as_bytes());
        e.checkpoint().unwrap();
        e.store().compact(&mut || true).unwrap();
    }
    assert_eq!(e.begin().get(b"k1").unwrap().as_deref(), Some(&b"v48"[..]));
    assert_eq!(
        e.begin().get(b"k2").unwrap().as_deref(),
        Some(&b"steady"[..])
    );
    let left = stored(&e, b"k1");
    assert!(left < held, "{left} versions of k1 left (held {held})");
    assert!(left <= 3, "{left} versions of k1 left");
}

/// A key deleted while its older versions lie in older runs: merges above
/// the last level keep its tombstone, so no snapshot reads an older
/// version as live (dropping it there fails this), and drop what is under
/// it in the runs they merge.
#[test]
fn a_deleted_key_stays_deleted_through_merges() {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(2));
    let e = Engine::open(p, Path::new("/db"), EngineOptions::default()).unwrap();
    let t = e.begin();
    assert!(t.add_engine(b"k", b"l", EngineKind::Lsm).unwrap());
    t.commit().unwrap();
    // A large last level, so the merges after stay above it.
    let mut t = e.begin();
    for i in 0..400u32 {
        t.put(format!("k5-{i:04}").as_bytes(), &[7; 1000]).unwrap();
    }
    t.commit().unwrap();
    e.checkpoint().unwrap();
    e.store().compact(&mut || true).unwrap();
    // k3's versions in runs of their own, merged into a tier above it.
    for i in 1..=4u32 {
        write(&e, b"k3", format!("v{i}").as_bytes());
        e.checkpoint().unwrap();
    }
    e.store().compact(&mut || true).unwrap();
    let mut t = e.begin();
    t.delete(b"k3").unwrap();
    t.commit().unwrap();
    for i in 0..60u32 {
        write(
            &e,
            format!("k4-{}", i % 7).as_bytes(),
            format!("{i}").as_bytes(),
        );
        e.checkpoint().unwrap();
        e.store().compact(&mut || true).unwrap();
        assert_eq!(e.begin().get(b"k3").unwrap(), None, "after round {i}");
    }
    // Left: the tombstone and the one version under it in a run it was
    // not merged with — both go when a merge reaches the last level.
    assert!(
        stored(&e, b"k3") <= 2,
        "{} entries of k3 left",
        stored(&e, b"k3")
    );
}
