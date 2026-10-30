//! Large transactions (PLAN.md §M14.11 L5b, §M14.20 V11): writes past the
//! memory budget spill into the store as intents; the transaction reads
//! them, commits them atomically (one status record: no write written
//! twice), or drops them — and a crash anywhere leaves a state some commit
//! left; vacuum makes committed intents versions and removes the rest.

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
    assert!(e.intent_count().unwrap() > 1000, "the writes spilled");
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
    // After it: all of it, its intents read as versions.
    let got = all(&e);
    assert_eq!(got.len(), 1 + 1999 + 2000);
    assert_eq!(got[&key(5)], b"again");
    assert_eq!(got[&key(7)], b"after the second spill");
    assert_eq!(got[&b"\x00first"[..]], b"zero-prefixed");
    assert!(!got.contains_key(&key(6)));
    assert!(e.intent_count().unwrap() > 0, "not written twice");
    // After reopening, the same; vacuum makes them versions.
    drop(before);
    drop(e);
    let e = engine_on(&p, opts()).unwrap();
    assert_eq!(all(&e), got);
    e.vacuum().unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
    assert_eq!(all(&e), got);
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
    assert!(e.intent_count().unwrap() > 0);
    t.rollback();
    assert_eq!(e.intent_count().unwrap(), 0);
    {
        let mut t = e.begin();
        for i in 0..1000 {
            t.put(&key(i), b"y").unwrap();
        }
    }
    assert_eq!(e.intent_count().unwrap(), 0);
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
    // Aborted: passed over by readers, then swept.
    assert!(all(&e).is_empty());
    let mut w = e.begin();
    w.put(&key(3), b"after").unwrap();
    w.commit().unwrap();
    e.vacuum().unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
    assert_eq!(all(&e).len(), 1);
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
    assert_eq!(e.intent_count().unwrap(), 0);
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
                e.vacuum().unwrap();
                assert!(
                    all(&e) == states[k],
                    "{mode:?} op {n}: vacuum changed the state"
                );
                assert_eq!(
                    e.intent_count().unwrap(),
                    0,
                    "{mode:?} op {n}: intents left after vacuum"
                );
                assert_eq!(e.side_count().unwrap(), 0, "{mode:?} op {n}: side area");
            }
        });
    }
}

/// Reads `(start, end)` a chunk at a time — chunks of at most `entries`
/// entries, ended early past `bytes`.
fn chunked(
    t: &nucleus_txn::Transaction,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
    entries: usize,
    bytes: usize,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut scan = t.scan(start, end).unwrap();
    let mut chunk = nucleus_txn::Chunk::new();
    let mut out = Vec::new();
    while scan.next_chunk(&mut chunk, entries, bytes).unwrap() {
        assert!(chunk.len() <= entries.max(1));
        out.extend(chunk.iter().map(|(k, v)| (k.to_vec(), v.to_vec())));
    }
    out
}

#[test]
fn chunked_scans_read_what_scans_read() {
    // Versions, deletions, a spilled transaction's side area, another
    // committed one's not yet resolved, and the reader's own writes: a
    // chunked scan of any range, in chunks of any size, gives the scan's
    // entries.
    let p = Arc::new(SimPlatform::new(7));
    let e = engine_on(&p, opts()).unwrap();
    let rng = SplitMix64::new(11);
    for round in 0..4 {
        let mut t = e.begin();
        for _ in 0..300 {
            let i = rng.below(500) as usize;
            if rng.below(4) == 0 {
                t.delete(&key(i)).unwrap();
            } else {
                t.put(&key(i), format!("r{round}-{i}").as_bytes()).unwrap();
            }
        }
        t.commit().unwrap();
    }
    let mut t = e.begin();
    for i in (0..500).step_by(3) {
        t.put(&key(i), b"own").unwrap();
    }
    for i in (0..500).step_by(7) {
        t.delete(&key(i)).unwrap();
    }
    let (lo, hi) = (key(40), key(410));
    for (start, end) in [
        (Bound::Unbounded, Bound::Unbounded),
        (Bound::Included(&lo[..]), Bound::Excluded(&hi[..])),
        (Bound::Excluded(&lo[..]), Bound::Included(&hi[..])),
    ] {
        let want: Vec<_> = t.scan(start, end).unwrap().map(Result::unwrap).collect();
        assert!(!want.is_empty());
        for (entries, bytes) in [
            (1, usize::MAX),
            (3, usize::MAX),
            (1024, 64),
            (1024, 1 << 20),
        ] {
            assert_eq!(
                chunked(&t, start, end, entries, bytes),
                want,
                "{entries}/{bytes}"
            );
        }
    }
}

#[test]
fn committed_intents_rank_with_versions_through_vacuum() {
    let p = Arc::new(SimPlatform::new(7));
    let e = engine_on(&p, opts()).unwrap();
    let mut seed = e.begin();
    seed.put(&key(1), b"v0").unwrap();
    seed.commit().unwrap();
    let at_v0 = e.begin();
    let mut late = e.begin(); // a writer from before the big commit
    // A large commit: key 1 (among many) an intent committed at its time.
    let mut big = e.begin();
    for i in 0..500 {
        big.put(&key(i), b"big").unwrap();
    }
    assert!(e.intent_count().unwrap() > 400);
    big.commit().unwrap();
    let at_big = e.begin();
    // A small commit over it: a version newer than the intent.
    let mut small = e.begin();
    small.put(&key(1), b"small").unwrap();
    small.commit().unwrap();
    let at_small = e.begin();
    let check = |what: &str| {
        assert_eq!(
            at_v0.get(&key(1)).unwrap().as_deref(),
            Some(&b"v0"[..]),
            "{what}"
        );
        assert_eq!(at_v0.get(&key(2)).unwrap(), None, "{what}");
        assert_eq!(
            at_big.get(&key(1)).unwrap().as_deref(),
            Some(&b"big"[..]),
            "{what}"
        );
        assert_eq!(
            at_small.get(&key(1)).unwrap().as_deref(),
            Some(&b"small"[..]),
            "{what}"
        );
        assert_eq!(
            at_small.get(&key(2)).unwrap().as_deref(),
            Some(&b"big"[..]),
            "{what}"
        );
        let seen: Vec<_> = at_big
            .scan(Bound::Included(&key(0)), Bound::Excluded(&key(3)))
            .unwrap()
            .map(|r| r.unwrap().1)
            .collect();
        assert_eq!(seen, vec![b"big".to_vec(); 3], "{what}");
        let seen: Vec<_> = at_small
            .scan(Bound::Included(&key(0)), Bound::Excluded(&key(3)))
            .unwrap()
            .map(|r| r.unwrap().1)
            .collect();
        assert_eq!(
            seen,
            vec![b"big".to_vec(), b"small".to_vec(), b"big".to_vec()],
            "{what}"
        );
    };
    check("intents");
    // A writer that began before the big commit conflicts with its intents.
    assert!(matches!(
        late.put(&key(7), b"late"),
        Err(Error::Conflict { .. })
    ));
    drop(late);
    e.vacuum().unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
    check("versions");
    drop((at_v0, at_big, at_small));
    let latest = all(&e);
    assert_eq!(latest[&key(1)], b"small");
    assert_eq!(latest.len(), 500);
}

#[test]
fn an_older_engines_side_area_is_moved_into_versions() {
    use nucleus_txn::mvcc::{encode_value, side_key, status_key};
    let p = Arc::new(SimPlatform::new(8));
    {
        let e = engine_on(&p, opts()).unwrap();
        let mut t = e.begin();
        t.put(&key(0), b"before").unwrap();
        t.commit().unwrap();
        // What an older engine left: transaction 5's side area committed
        // at 9 (its status record), transaction 6's never committed.
        let mut ops = Vec::new();
        for i in 0..50 {
            ops.push((side_key(5, &key(i)), Some(encode_value(Some(b"old")))));
            ops.push((
                side_key(6, &key(i + 100)),
                Some(encode_value(Some(b"gone"))),
            ));
        }
        ops.push((status_key(5), Some(2u64.to_be_bytes().to_vec())));
        let lsn = e
            .store()
            .write(&nucleus_storage::Batch { meta: 2, ops })
            .unwrap();
        e.store().wait_durable(lsn).unwrap();
    }
    let e = engine_on(&p, opts()).unwrap();
    assert_eq!(e.side_count().unwrap(), 0);
    let got = all(&e);
    assert_eq!(got.len(), 50);
    assert_eq!(got[&key(0)], b"old");
    assert!(!got.contains_key(&key(100)));
    // New ids are past the old ones.
    let mut t = e.begin();
    for i in 0..300 {
        t.put(&key(i + 1000), b"new").unwrap();
    }
    t.commit().unwrap();
    assert_eq!(all(&e).len(), 350);
}

#[test]
fn a_stopped_vacuum_goes_on_where_it_stopped() {
    let p = Arc::new(SimPlatform::new(11));
    let e = engine_on(&p, opts()).unwrap();
    let mut big = e.begin();
    for i in 0..600 {
        big.put(&key(i), b"big").unwrap();
    }
    big.commit().unwrap();
    let intents = e.intent_count().unwrap();
    assert!(intents > 500);
    assert!(e.intents_pending());
    // Stopped after its first batch: some intents made versions, the
    // status kept.
    let mut batches = 0;
    e.vacuum_paced(64, &mut || {
        batches += 1;
        false
    })
    .unwrap();
    assert_eq!(batches, 1);
    let left = e.intent_count().unwrap();
    assert!(left < intents && left > 0, "{left} of {intents}");
    assert!(e.intents_pending());
    // A transaction committing meanwhile, its writes among the rest.
    let mut small = e.begin();
    small.put(&key(5), b"small").unwrap();
    small.put(&key(590), b"small").unwrap();
    small.commit().unwrap();
    // Gone on from there to the end: every intent a version, the status
    // gone, every value as committed.
    e.vacuum_paced(64, &mut || true).unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
    assert!(!e.intents_pending());
    let rows = all(&e);
    assert_eq!(rows.len(), 600);
    for i in 0..600 {
        let want: &[u8] = if i == 5 || i == 590 { b"small" } else { b"big" };
        assert_eq!(rows[&key(i)], want, "key {i}");
    }
    let before = e.activity();
    assert_eq!(e.running(), 0);
    let r = e.begin();
    assert_eq!((e.activity(), e.running()), (before + 1, 1));
    drop(r);
    assert_eq!(e.running(), 0);
}
