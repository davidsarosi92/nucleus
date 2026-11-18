//! `Transaction::get_many` gives what a `get` each gives (issue #1: a
//! group of keys sought together, their descents' misses overlapped):
//! keys with versions newer and older than the reader's snapshot,
//! tombstones, keys never written, keys holding zero bytes (escaped in
//! the store), the reader's own writes, and other transactions' intents
//! in the store (their writes spilled).

use std::path::Path;
use std::sync::Arc;

use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Transaction};

fn engine() -> Engine {
    let p = Arc::new(SimPlatform::new(11));
    let opts = EngineOptions {
        store: StoreOptions {
            cache_pages: 256,
            wal_segment_size: 1 << 16,
            ..Default::default()
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        // Small: a big transaction's writes spill, its intents entering
        // the store.
        txn_memory: 4096,
        wal_minimal: true,
    };
    Engine::open(p as Arc<dyn Platform>, Path::new("/db"), opts).unwrap()
}

/// Keys with zeros in them, of a few lengths: escaped in the store.
fn key(i: u64) -> Vec<u8> {
    let mut k = vec![0, 1, 0];
    k.extend_from_slice(&(i * 3).to_be_bytes());
    if i.is_multiple_of(5) {
        k.push(0);
    }
    k
}

fn check(t: &Transaction, rng: &SplitMix64, n: u64) {
    let mut out = Vec::new();
    for _ in 0..60 {
        let size = 1 + rng.below(12) as usize;
        let keys: Vec<Vec<u8>> = (0..size).map(|_| key(rng.below(n + 20))).collect();
        let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        t.get_many(&refs, &mut out).unwrap();
        let one: Vec<_> = refs.iter().map(|k| t.get(k).unwrap()).collect();
        assert_eq!(out, one, "{keys:?}");
    }
}

#[test]
fn get_many_is_a_get_each() {
    let e = engine();
    let rng = SplitMix64::new(3);
    let n = 3000;
    let mut t = e.begin();
    for i in 0..n {
        t.put(&key(i), format!("v0-{i}").as_bytes()).unwrap();
    }
    t.commit().unwrap();
    let mut readers = Vec::new();
    for round in 1..6 {
        // A reader at each snapshot, kept open: what it sees stays.
        readers.push(e.begin());
        let mut t = e.begin();
        for _ in 0..400 {
            let i = rng.below(n);
            if rng.below(4) == 0 {
                t.delete(&key(i)).unwrap();
            } else {
                t.put(&key(i), format!("v{round}-{i}").as_bytes()).unwrap();
            }
        }
        t.commit().unwrap();
    }
    for r in &readers {
        check(r, &rng, n);
    }
    // A writer's own writes, and another writer's intents in the store.
    let mut other = e.begin();
    for i in (0..n).step_by(3) {
        other.put(&key(i), b"intent, not committed").unwrap();
    }
    let mut mine = e.begin();
    // (Keys the other writer does not hold: a write there conflicts.)
    for i in (1..n).step_by(7).filter(|i| !i.is_multiple_of(3)) {
        mine.put(&key(i), b"mine").unwrap();
    }
    check(&mine, &rng, n);
    check(&e.begin(), &rng, n);
    drop(other);
    check(&e.begin(), &rng, n);
}
