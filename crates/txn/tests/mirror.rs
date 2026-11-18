//! Mirrored prefixes ([`Engine::mirror`], PLAN.md §M14.28 L3b): a write
//! under a source prefix is written under its destination too, unchecked
//! there; a copy's direct write there is checked, and loses to a mirrored
//! write committed after it began.

use std::path::Path;
use std::sync::Arc;

use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_txn::{Engine, EngineOptions};

fn engine(opts: EngineOptions) -> Engine {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(1));
    Engine::open(p, Path::new("/db"), opts).unwrap()
}

fn get(e: &Engine, k: &[u8]) -> Option<Vec<u8>> {
    e.begin().get(k).unwrap()
}

#[test]
fn writes_are_mirrored_while_the_guard_lives() {
    let e = engine(EngineOptions::default());
    let mut before = e.begin();
    before.put(b"s/early", b"0").unwrap();
    let m = e.mirror(vec![(b"s/".to_vec(), b"d/".to_vec())]).unwrap();
    // Overlapping prefixes are refused, either way round.
    assert!(e.mirror(vec![(b"s/x".to_vec(), b"e/".to_vec())]).is_err());
    assert!(e.mirror(vec![(b"f/".to_vec(), b"d".to_vec())]).is_err());
    assert!(e.mirror(vec![(b"f/".to_vec(), b"f/g".to_vec())]).is_err());
    // A write of a transaction begun before, made now: mirrored; the one
    // it made before is not.
    before.put(b"s/late", b"1").unwrap();
    before.commit().unwrap();
    assert_eq!(get(&e, b"d/late").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&e, b"d/early"), None);
    // Puts and deletes alike; keys outside the source are not.
    let mut t = e.begin();
    t.put(b"s/a", b"2").unwrap();
    t.put(b"t/a", b"3").unwrap();
    t.delete(b"s/late").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&e, b"d/a").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&e, b"d/late"), None);
    assert_eq!(get(&e, b"t/a").as_deref(), Some(&b"3"[..]));
    // A copy that read the source before a mirrored write committed
    // loses to it; a mirrored write after the copy's commit wins.
    let mut copy = e.begin();
    let seen = copy.get(b"s/a").unwrap().unwrap();
    let mut w = e.begin();
    w.put(b"s/a", b"4").unwrap();
    w.commit().unwrap();
    copy.put(b"d/a", &seen).unwrap_err();
    let mut copy = e.begin();
    let seen = copy.get(b"s/a").unwrap().unwrap();
    let mut w = e.begin();
    w.put(b"s/a", b"5").unwrap();
    copy.put(b"d/a", &seen).unwrap();
    copy.commit().unwrap();
    w.commit().unwrap();
    assert_eq!(get(&e, b"d/a").as_deref(), Some(&b"5"[..]));
    // Dropped: no more mirroring.
    drop(m);
    let mut t = e.begin();
    t.put(b"s/b", b"6").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&e, b"d/b"), None);
    assert!(e.mirror(vec![(b"s/".to_vec(), b"d/".to_vec())]).is_ok());
}

#[test]
fn a_spilled_transaction_mirrors_too() {
    let e = engine(EngineOptions {
        txn_memory: 4096,
        ..EngineOptions::default()
    });
    let _m = e.mirror(vec![(b"s/".to_vec(), b"d/".to_vec())]).unwrap();
    let mut t = e.begin();
    for i in 0..2000u32 {
        t.put(&[b"s/".as_slice(), &i.to_be_bytes()].concat(), &[7; 40])
            .unwrap();
    }
    t.commit().unwrap();
    let r = e.begin();
    for i in 0..2000u32 {
        assert_eq!(
            r.get(&[b"d/".as_slice(), &i.to_be_bytes()].concat())
                .unwrap()
                .as_deref(),
            Some(&[7u8; 40][..])
        );
    }
}
