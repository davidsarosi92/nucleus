//! `Transaction::first` answers from memory inside a range it found the
//! store empty in (PLAN.md §M14.20 W3): whatever the transaction writes,
//! deletes and spills meanwhile, and whatever others commit, it gives what
//! a scan gives.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions};

fn engine() -> Engine {
    let p = Arc::new(SimPlatform::new(7));
    let opts = EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        // Small: the writes spill now and then, their intents entering
        // the store.
        txn_memory: 4096,
        wal_minimal: true,
    };
    Engine::open(p as Arc<dyn Platform>, Path::new("/db"), opts).unwrap()
}

fn key(i: u64) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

fn bound(rng: &mut SplitMix64) -> Bound<Vec<u8>> {
    match rng.next_u64() % 5 {
        0 => Bound::Unbounded,
        1 | 2 => Bound::Included(key(rng.next_u64() % 400)),
        _ => Bound::Excluded(key(rng.next_u64() % 400)),
    }
}

#[test]
fn first_is_a_scan_s_first_whatever_happens_meanwhile() {
    let e = engine();
    let mut rng = SplitMix64::new(42);
    // Some committed keys before.
    let mut t = e.begin();
    for i in (0..400).step_by(7) {
        t.put(&key(i), b"old").unwrap();
    }
    t.commit().unwrap();
    for round in 0..40 {
        let mut t = e.begin();
        for step in 0..400 {
            match rng.next_u64() % 10 {
                // (A key another committed meanwhile conflicts: refused,
                // nothing written.)
                0..=2 => {
                    let _ = t.put(
                        &key(rng.next_u64() % 400),
                        format!("{round}.{step}").as_bytes(),
                    );
                }
                3 => {
                    let _ = t.delete(&key(rng.next_u64() % 400));
                }
                4 => {
                    // Another transaction's commit: after this one's
                    // snapshot, so never seen.
                    let mut o = e.begin();
                    // (Refused on a key this one's spilled intents hold.)
                    if o.put(&key(rng.next_u64() % 400), b"other").is_ok() {
                        let _ = o.commit();
                    }
                }
                _ => {
                    // Ascending looks, as a load's, and random ones.
                    let (s, end) = if rng.next_u64().is_multiple_of(2) {
                        // A narrow window, often empty: remembered.
                        let at = rng.next_u64() % 400;
                        (
                            Bound::Included(key(at)),
                            Bound::Excluded(key(at + 1 + rng.next_u64() % 6)),
                        )
                    } else {
                        (bound(&mut rng), bound(&mut rng))
                    };
                    let (s, end) = (
                        s.as_ref().map(Vec::as_slice),
                        end.as_ref().map(Vec::as_slice),
                    );
                    let got = t.first(s, end).unwrap();
                    let want = t.scan(s, end).unwrap().next().transpose().unwrap();
                    assert_eq!(got, want, "round {round} step {step}: {s:?} {end:?}");
                    // The same look again (from memory, if it found a gap).
                    assert_eq!(
                        t.first(s, end).unwrap(),
                        want,
                        "round {round} step {step} again"
                    );
                }
            }
        }
        if round % 2 == 0 {
            let _ = t.commit();
        }
    }
}

/// A range found empty, then written into and spilled: its intents are in
/// the store, which the transaction reads again (it forgot the range).
#[test]
fn a_remembered_range_sees_its_own_writes_after_they_spill() {
    let e = engine();
    let mut t = e.begin();
    let (lo, hi) = (key(10), key(20));
    let range = || {
        (
            Bound::Included(lo.as_slice()),
            Bound::Excluded(hi.as_slice()),
        )
    };
    assert_eq!(t.first(range().0, range().1).unwrap(), None);
    t.put(&key(15), b"mine").unwrap();
    assert_eq!(
        t.first(range().0, range().1).unwrap(),
        Some((key(15), b"mine".to_vec()))
    );
    // Writes elsewhere, past the memory allowed: everything spills.
    for i in 100..400 {
        t.put(&key(i), b"filler").unwrap();
    }
    assert_eq!(
        t.first(range().0, range().1).unwrap(),
        Some((key(15), b"mine".to_vec()))
    );
    t.delete(&key(15)).unwrap();
    assert_eq!(t.first(range().0, range().1).unwrap(), None);
}
