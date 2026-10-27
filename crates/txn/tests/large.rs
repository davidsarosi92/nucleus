//! Large transactions (PLAN.md §M14.11 L5b): writes past the memory budget
//! spill into the store's side area; the transaction reads them, commits
//! them atomically, or drops them — and a crash anywhere leaves a state
//! some commit left.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, Isolation, TxnOptions};

/// A budget small enough that a few dozen writes spill.
fn opts() -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        txn_memory: 2048,
    }
}

fn engine_on(p: &Arc<SimPlatform>, opts: EngineOptions) -> Result<Engine, Error> {
    Engine::open(Arc::clone(p) as Arc<dyn Platform>, Path::new("/db"), opts)
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

fn all(e: &Engine) -> BTreeMap<Vec<u8>, Vec<u8>> {
    e.begin()
        .scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_transaction_past_its_budget_reads_and_commits_its_writes() {
    let p = Arc::new(SimPlatform::new(1));
    let e = engine_on(&p, opts()).unwrap();
    let mut t = e.begin();
    t.put(b"\x00first", b"zero-prefixed").unwrap();
    for i in 0..2000 {
        t.put(&key(i), format!("v{i}").as_bytes()).unwrap();
    }
    assert!(e.side_count().unwrap() > 1000, "the writes spilled");
    // Its own writes, spilled or not: overwritten, deleted, read back.
    t.put(&key(5), b"again").unwrap();
    t.delete(&key(6)).unwrap();
    for i in 0..2000 {
        t.put(&key(i + 10_000), b"more").unwrap();
    }
    t.put(&key(7), b"after the second spill").unwrap();
    assert_eq!(t.get(&key(5)).unwrap().as_deref(), Some(&b"again"[..]));
    assert_eq!(t.get(&key(6)).unwrap(), None);
    assert_eq!(t.get(&key(1999)).unwrap().as_deref(), Some(&b"v1999"[..]));
    let seen: Vec<_> = t
        .scan(Bound::Included(&key(4)), Bound::Excluded(&key(9)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        seen,
        vec![
            (key(4), b"v4".to_vec()),
            (key(5), b"again".to_vec()),
            (key(7), b"after the second spill".to_vec()),
            (key(8), b"v8".to_vec()),
        ]
    );
    // Others see none of it before the commit.
    assert!(all(&e).is_empty());
    let before = e.begin();
    t.commit().unwrap();
    assert!(
        before
            .scan(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .next()
            .is_none()
    );
    // After it: all of it, moved into versions.
    let got = all(&e);
    assert_eq!(got.len(), 1 + 1999 + 2000);
    assert_eq!(got[&key(5)], b"again");
    assert_eq!(got[&key(7)], b"after the second spill");
    assert_eq!(got[&b"\x00first"[..]], b"zero-prefixed");
    assert!(!got.contains_key(&key(6)));
    assert_eq!(e.side_count().unwrap(), 0);
    // And after reopening.
    drop(before);
    drop(e);
    let e = engine_on(&p, opts()).unwrap();
    assert_eq!(all(&e), got);
}

#[test]
fn rolled_back_or_dropped_spills_leave_nothing() {
    let p = Arc::new(SimPlatform::new(2));
    let e = engine_on(&p, opts()).unwrap();
    let mut t = e.begin();
    for i in 0..1000 {
        t.put(&key(i), b"x").unwrap();
    }
    assert!(e.side_count().unwrap() > 0);
    t.rollback();
    assert_eq!(e.side_count().unwrap(), 0);
    {
        let mut t = e.begin();
        for i in 0..1000 {
            t.put(&key(i), b"y").unwrap();
        }
    }
    assert_eq!(e.side_count().unwrap(), 0);
    assert!(all(&e).is_empty());
    // A crash with spilled writes: dropped on opening.
    let mut t = e.begin();
    for i in 0..1000 {
        t.put(&key(i), b"z").unwrap();
    }
    std::mem::forget(t);
    drop(e);
    p.sim_fs().crash();
    let e = engine_on(&p, opts()).unwrap();
    assert_eq!(e.side_count().unwrap(), 0);
    assert!(all(&e).is_empty());
}

#[test]
fn conflicts_with_spilled_writes_either_way() {
    let p = Arc::new(SimPlatform::new(3));
    let e = engine_on(&p, opts()).unwrap();
    let mut big = e.begin();
    for i in 0..1000 {
        big.put(&key(i), b"big").unwrap();
    }
    // A spilled key: another writer conflicts with it (at the write, and
    // at the commit if it got past that).
    let mut small = e.begin();
    assert!(matches!(
        small.put(&key(3), b"small"),
        Err(Error::Conflict { .. })
    ));
    let mut other = e.begin();
    other.put(&key(5000), b"other").unwrap();
    other.commit().unwrap();
    // A key someone committed after the big one began: its spill (or its
    // commit) fails.
    let mut c = e.begin();
    c.put(&key(4000), b"c").unwrap();
    c.commit().unwrap();
    let mut failed = false;
    for i in 1000..3000 {
        if big
            .put(&key(if i == 1500 { 4000 } else { i }), b"big")
            .is_err()
        {
            failed = true;
            break;
        }
    }
    if failed {
        drop(big); // doomed: its spilled writes go with it
    } else {
        assert!(big.commit().is_err());
    }
    assert_eq!(e.side_count().unwrap(), 0);
    assert_eq!(all(&e).len(), 2);
}

#[test]
fn serializable_reads_validate_against_a_large_commit() {
    let p = Arc::new(SimPlatform::new(4));
    let e = engine_on(&p, opts()).unwrap();
    let reader = e
        .begin_with(TxnOptions {
            isolation: Isolation::Serializable,
            ..TxnOptions::default()
        })
        .unwrap();
    assert_eq!(reader.get(&key(500)).unwrap(), None);
    let mut big = e.begin();
    for i in 0..1000 {
        big.put(&key(i), b"big").unwrap();
    }
    big.commit().unwrap();
    let mut reader = reader;
    reader.put(b"elsewhere", b"x").unwrap();
    assert!(matches!(reader.commit(), Err(Error::Conflict { .. })));
}

#[test]
fn readers_see_a_large_commit_all_at_once() {
    let p = Arc::new(SimPlatform::new(5));
    let e = engine_on(&p, opts()).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (e, done) = (e.clone(), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut saw_all = false;
            while !done.load(Ordering::Acquire) || !saw_all {
                let n = e
                    .begin()
                    .scan(Bound::Unbounded, Bound::Unbounded)
                    .unwrap()
                    .count();
                assert!(n == 0 || n == 5000, "a partial commit: {n} keys");
                saw_all = n == 5000;
            }
        })
    };
    let mut t = e.begin();
    for i in 0..5000 {
        t.put(&key(i), b"v").unwrap();
    }
    t.commit().unwrap();
    done.store(true, Ordering::Release);
    reader.join().unwrap();
}

#[test]
fn crash_at_every_io_point_with_large_transactions() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("txn_large_crash_at_every_io_point", 2, |seed| {
            type Model = BTreeMap<Vec<u8>, Vec<u8>>;
            let run = |p: &Arc<SimPlatform>| -> (Vec<Model>, usize) {
                let rng = SplitMix64::new(seed);
                let mut states = vec![Model::new()];
                let mut acked = 0;
                let _ = (|| -> Result<(), Error> {
                    let e = engine_on(p, opts())?;
                    let mut model = Model::new();
                    for i in 1..=4 {
                        let mut t = e.begin();
                        // Every other transaction is large enough to spill.
                        let n = if i % 2 == 0 { 120 } else { 5 };
                        for _ in 0..n {
                            let k = key(rng.below(150) as usize);
                            if rng.below(4) == 0 {
                                t.delete(&k)?;
                                model.remove(&k);
                            } else {
                                t.put(&k, &[i as u8; 40])?;
                                model.insert(k, vec![i as u8; 40]);
                            }
                        }
                        states.push(model.clone());
                        t.commit()?;
                        acked = i;
                    }
                    Ok(())
                })();
                (states, acked)
            };
            let dry = Arc::new(SimPlatform::new(seed));
            let (states, _) = run(&dry);
            let total = dry.sim_fs().op_count();
            for n in (1..=total).step_by(3) {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = run(&p);
                let e =
                    engine_on(&p, opts()).unwrap_or_else(|err| panic!("{mode:?} op {n}: {err}"));
                let k: usize = e.visible().to_string().parse().unwrap();
                assert!(
                    k >= acked && k < states.len(),
                    "{mode:?} op {n}: at {k}, acked {acked}"
                );
                assert!(
                    all(&e) == states[k],
                    "{mode:?} op {n}: state is not that of commit {k}"
                );
                assert_eq!(
                    e.side_count().unwrap(),
                    0,
                    "{mode:?} op {n}: side area left"
                );
            }
        });
    }
}
