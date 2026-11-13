//! Writes into fresh ranges without the WAL (PLAN.md D16, §M14.20 W4):
//! values a transaction puts into a range it declared fresh go into the
//! store as final versions, unlogged; its commit checkpoints before the
//! commit record, and a rollback — or a crash before the commit — deletes
//! the range whole.  Declined when the store holds an entry in the range or
//! another running transaction writes it so; no one else may put an entry
//! into a range being written so.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error};

fn opts() -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        txn_memory: 64 << 20,
        wal_minimal: true,
    }
}

fn engine_on(p: &Arc<SimPlatform>, opts: EngineOptions) -> Result<Engine, Error> {
    Engine::open(Arc::clone(p) as Arc<dyn Platform>, Path::new("/db"), opts)
}

/// The fresh range: `i…`.
const START: &[u8] = b"i";
const END: &[u8] = b"j";

fn ikey(i: usize) -> Vec<u8> {
    format!("i{i:06}").into_bytes()
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn all(e: &Engine) -> Model {
    e.begin()
        .scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The marks of unlogged ranges in the store (`00 04 …`).
fn marks(e: &Engine) -> usize {
    let tree = e.store().read().unwrap();
    tree.range(Bound::Included(&[0, 4][..]), Bound::Excluded(&[0, 5][..]))
        .unwrap()
        .count()
}

/// Every store entry in the fresh range (any version, any intent).
fn stored_in_range(e: &Engine) -> usize {
    let tree = e.store().read().unwrap();
    tree.range(Bound::Included(START), Bound::Excluded(END))
        .unwrap()
        .count()
}

#[test]
fn fresh_writes_skip_the_wal_and_are_durable_at_the_commit() {
    let run = |wal_minimal: bool| {
        let p = Arc::new(SimPlatform::new(1));
        let e = engine_on(
            &p,
            EngineOptions {
                wal_minimal,
                ..opts()
            },
        )
        .unwrap();
        let wal_before = e.store().wal_since_checkpoint();
        let mut t = e.begin();
        t.fresh(START, END);
        for i in 0..5000 {
            t.put(&ikey(i), &[7; 50]).unwrap();
        }
        // Its own reads see them, pending or in the store.
        assert_eq!(t.get(&ikey(4999)).unwrap().as_deref(), Some(&[7; 50][..]));
        assert_eq!(
            t.scan(Bound::Included(START), Bound::Excluded(END))
                .unwrap()
                .count(),
            5000
        );
        t.commit().unwrap();
        // (The unlogged commit checkpoints: what is left is its record.)
        let wal = e.store().wal_since_checkpoint().saturating_sub(wal_before);
        assert_eq!(e.intent_count().unwrap(), 0, "versions, not intents");
        assert_eq!(marks(&e), 0);
        assert_eq!(all(&e).len(), 5000);
        // Durable: a crash after the commit keeps it.
        drop(e);
        p.sim_fs().crash();
        let e = engine_on(&p, opts()).unwrap();
        assert_eq!(all(&e).len(), 5000);
        assert_eq!(e.version_count().unwrap(), 5000);
        wal
    };
    let (minimal, full) = (run(true), run(false));
    assert!(minimal < 1024, "unlogged: {minimal} bytes of WAL");
    assert!(full > 5000 * 50, "logged: {full} bytes of WAL");
}

#[test]
fn a_rollback_or_a_crash_before_the_commit_deletes_the_range() {
    let p = Arc::new(SimPlatform::new(2));
    let e = engine_on(&p, opts()).unwrap();
    let mut w = e.begin();
    w.put(&key(1), b"kept").unwrap();
    w.commit().unwrap();
    let load = |e: &Engine, checkpoint_midway: bool| {
        let mut t = e.begin();
        t.fresh(START, END);
        for i in 0..3000 {
            t.put(&ikey(i), &[1; 40]).unwrap();
            if checkpoint_midway && i == 1500 {
                // Half of it in the checkpoint, the rest after.
                t.get(&ikey(0)).unwrap();
                e.checkpoint().unwrap();
            }
        }
        t.get(&ikey(0)).unwrap(); // in the store
        t
    };
    load(&e, false).rollback();
    assert_eq!(stored_in_range(&e), 0);
    assert_eq!(marks(&e), 0);
    drop(load(&e, true)); // dropped: rolled back
    assert_eq!(stored_in_range(&e), 0);
    let mut e = e;
    for checkpoint_midway in [false, true] {
        std::mem::forget(load(&e, checkpoint_midway));
        drop(e);
        p.sim_fs().crash();
        e = engine_on(&p, opts()).unwrap();
        assert_eq!(stored_in_range(&e), 0, "midway {checkpoint_midway}");
        assert_eq!(marks(&e), 0);
        assert_eq!(all(&e).len(), 1);
    }
    // The range is free again.
    let mut t = e.begin();
    t.fresh(START, END);
    t.put(&ikey(3), b"x").unwrap();
    t.commit().unwrap();
    assert_eq!(all(&e).len(), 2);
}

#[test]
fn declined_where_the_store_holds_an_entry_or_another_writes_the_range() {
    let p = Arc::new(SimPlatform::new(3));
    let e = engine_on(&p, opts()).unwrap();
    // A snapshot older than a commit into the range: it sees the range
    // empty, but its rollback must not delete the commit's entry.
    let mut stale = e.begin();
    let mut w = e.begin();
    w.put(&ikey(500), b"committed").unwrap();
    w.commit().unwrap();
    stale.fresh(START, END);
    for i in 0..100 {
        stale.put(&ikey(i), b"stale").unwrap();
    }
    assert_eq!(marks(&e), 0, "declined: logged");
    stale.rollback();
    assert_eq!(all(&e).len(), 1);
    let mut d = e.begin();
    d.delete(&ikey(500)).unwrap();
    d.commit().unwrap();
    e.vacuum().unwrap();
    assert_eq!(stored_in_range(&e), 0);

    // Two transactions declaring overlapping ranges: the second's writes
    // are logged, and its commit — a key inside the first's range —
    // conflicts while the first runs.
    let mut a = e.begin();
    a.fresh(START, END);
    a.put(&ikey(1), b"a").unwrap();
    assert_eq!(marks(&e), 1);
    let mut b = e.begin();
    b.fresh(b"i000100", b"i000200");
    b.put(b"i000150", b"b").unwrap();
    assert_eq!(marks(&e), 1, "the second declined");
    assert!(matches!(b.commit(), Err(Error::Conflict { .. })));
    // Nor may a plain write land there, committed or spilled.
    let mut c = e.begin();
    c.put(&ikey(9), b"c").unwrap();
    assert!(matches!(c.commit(), Err(Error::Conflict { .. })));
    a.commit().unwrap();
    assert_eq!(all(&e).len(), 1);
    assert_eq!(marks(&e), 0);
    // After the commit, the range is an ordinary one.
    let mut c = e.begin();
    c.put(&ikey(9), b"c").unwrap();
    c.commit().unwrap();
    assert_eq!(all(&e).len(), 2);
}

/// `put_new`'s run of keys found without an entry (`Gap`) is forgotten
/// where an unlogged write enters it: the write may reach the store before
/// the next `put_new` of its key (here by a read).
#[test]
fn put_new_sees_an_unlogged_write_inside_a_run_it_found_empty() {
    let p = Arc::new(SimPlatform::new(5));
    let e = engine_on(&p, opts()).unwrap();
    let mut t = e.begin();
    t.fresh(START, END);
    // The store is empty from `h` on: the run reaches past the range.
    assert!(t.put_new(b"h", b"x").unwrap());
    t.put(&ikey(1), b"unlogged").unwrap();
    t.get(b"z").unwrap(); // writes it into the store
    assert!(!t.put_new(&ikey(1), b"again").unwrap());
    assert_eq!(t.get(&ikey(1)).unwrap().as_deref(), Some(&b"unlogged"[..]));
    t.commit().unwrap();
    assert_eq!(all(&e).len(), 2);
}

#[test]
fn spills_into_a_range_written_unlogged_conflict() {
    let p = Arc::new(SimPlatform::new(4));
    let e = engine_on(
        &p,
        EngineOptions {
            txn_memory: 2048,
            ..opts()
        },
    )
    .unwrap();
    let mut a = e.begin();
    a.fresh(START, END);
    a.put(&ikey(1), b"a").unwrap();
    let mut b = e.begin();
    let mut failed = false;
    for i in 100..400 {
        if let Err(err) = b.put(&ikey(i), &[2; 40]) {
            assert!(matches!(err, Error::Conflict { .. }), "{err}");
            failed = true;
            break;
        }
    }
    assert!(failed, "the spill conflicted");
    drop(b);
    a.rollback();
    assert_eq!(stored_in_range(&e), 0);
    assert_eq!(e.intent_count().unwrap(), 0);
}

/// A transaction's own view of a range it writes unlogged — in order and
/// out of it, overwritten, deleted, `put_new`, spilled now and then —
/// against a model, through `get`, `scan`, `first`; then what commits.
#[test]
fn own_reads_and_writes_of_an_unlogged_range_match_a_model() {
    harness::run("txn_unlogged_model", 8, |seed| {
        let rng = SplitMix64::new(seed);
        let p = Arc::new(SimPlatform::new(seed));
        let e = engine_on(
            &p,
            EngineOptions {
                txn_memory: 4096,
                ..opts()
            },
        )
        .unwrap();
        let mut model = Model::new();
        // Something outside the range, committed.
        let mut w = e.begin();
        for i in 0..20 {
            w.put(&key(i), b"out").unwrap();
            model.insert(key(i), b"out".to_vec());
        }
        w.commit().unwrap();
        let mut t = e.begin();
        t.fresh(START, END);
        let mut next = 0;
        for step in 0..600 {
            let v = format!("v{step}").into_bytes();
            match rng.below(12) {
                // Mostly in order, as a load writes.
                0..=5 => {
                    next += 1 + rng.below(3) as usize;
                    t.put(&ikey(next), &v).unwrap();
                    model.insert(ikey(next), v);
                }
                6 => {
                    let k = ikey(rng.below(next as u64 + 5) as usize);
                    t.put(&k, &v).unwrap();
                    model.insert(k, v);
                }
                7 => {
                    let k = ikey(rng.below(next as u64 + 5) as usize);
                    t.delete(&k).unwrap();
                    model.remove(&k);
                }
                8 => {
                    let k = if rng.below(2) == 0 {
                        next += 1;
                        ikey(next)
                    } else {
                        ikey(rng.below(next as u64 + 5) as usize)
                    };
                    let fresh = !model.contains_key(&k);
                    assert_eq!(t.put_new(&k, &v).unwrap(), fresh, "put_new {k:?}");
                    if fresh {
                        model.insert(k, v);
                    }
                }
                9 => {
                    let k = ikey(rng.below(next as u64 + 5) as usize);
                    assert_eq!(t.get(&k).unwrap().as_ref(), model.get(&k), "get {k:?}");
                }
                10 => {
                    let from = ikey(rng.below(next as u64 + 5) as usize);
                    let got = t
                        .first(Bound::Included(&from), Bound::Excluded(END))
                        .unwrap();
                    let want = model
                        .range(from.clone()..END.to_vec())
                        .next()
                        .map(|(k, v)| (k.clone(), v.clone()));
                    assert_eq!(got, want, "first from {from:?}");
                }
                _ => {
                    let got: Model = t
                        .scan(Bound::Unbounded, Bound::Unbounded)
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                    assert!(got == model, "scan at step {step}");
                }
            }
        }
        t.commit().unwrap();
        assert!(all(&e) == model, "committed");
        assert_eq!(marks(&e), 0);
        drop(e);
        p.sim_fs().crash();
        let e = engine_on(&p, opts()).unwrap();
        assert!(all(&e) == model, "after a crash");
        e.vacuum().unwrap();
        assert!(all(&e) == model, "after vacuum");
        assert_eq!(e.intent_count().unwrap(), 0);
    });
}

#[test]
fn crash_at_every_io_point_of_unlogged_loads() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("txn_unlogged_crash_at_every_io_point", 2, |seed| {
            let run = |p: &Arc<SimPlatform>| -> (Vec<Model>, usize) {
                let rng = SplitMix64::new(seed);
                let mut states = vec![Model::new()];
                let mut acked = 0;
                let _ = (|| -> Result<(), Error> {
                    let e = engine_on(p, opts())?;
                    let mut model = Model::new();
                    for i in 1..=4usize {
                        let mut t = e.begin();
                        // A small logged write, and every other transaction
                        // a load of a fresh range of its own (rolled back
                        // once).
                        let k = key(rng.below(50) as usize);
                        t.put(&k, &[i as u8; 8])?;
                        model.insert(k, vec![i as u8; 8]);
                        if i % 2 == 0 {
                            let (s, end) = (format!("i{i}"), format!("i{}", i + 1));
                            t.fresh(s.as_bytes(), end.as_bytes());
                            // Past the cache: pages written back before
                            // the commit.
                            for j in 0..3000 {
                                let k = format!("i{i}{j:05}").into_bytes();
                                t.put(&k, &[i as u8; 60])?;
                                if i == 4 {
                                    model.insert(k, vec![i as u8; 60]);
                                }
                            }
                            if i == 2 {
                                t.rollback();
                                // (The small write went with it.)
                                model = states.last().unwrap().clone();
                                continue;
                            }
                        }
                        states.push(model.clone());
                        t.commit()?;
                        acked += 1;
                    }
                    Ok(())
                })();
                (states, acked)
            };
            let dry = Arc::new(SimPlatform::new(seed));
            let (states, _) = run(&dry);
            let total = dry.sim_fs().op_count();
            assert!(total > 50, "{total} I/O operations");
            for n in 1..=total {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = run(&p);
                let e =
                    engine_on(&p, opts()).unwrap_or_else(|err| panic!("{mode:?} op {n}: {err}"));
                // (States by commit timestamp: the rolled-back transaction
                // has none.)
                let k: usize = e.visible().to_string().parse().unwrap();
                assert!(
                    k >= acked && k < states.len(),
                    "{mode:?} op {n}: at {k}, acked {acked}"
                );
                assert!(
                    all(&e) == states[k],
                    "{mode:?} op {n}: state is not that of commit {k}"
                );
                assert_eq!(marks(&e), 0, "{mode:?} op {n}: marks left");
                e.vacuum().unwrap();
                assert!(all(&e) == states[k], "{mode:?} op {n}: vacuum");
                assert_eq!(e.intent_count().unwrap(), 0, "{mode:?} op {n}: intents");
            }
        });
    }
}
