//! Store tests: WAL replay, checkpoints, torn logs, poisoning, and a crash
//! at every I/O point of a workload of batches and checkpoints.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::real::RealPlatform;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{OpenMode, Platform, Rng, SplitMix64};
use nucleus_storage::{Batch, Error, Store, StoreOptions};

const DIR: &str = "/db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn opts() -> StoreOptions {
    StoreOptions {
        cache_pages: 8,
        wal_segment_size: 2048,
    }
}

fn sim(seed: u64) -> Arc<SimPlatform> {
    Arc::new(SimPlatform::new(seed))
}

fn open(p: &Arc<SimPlatform>) -> Result<(Store, nucleus_storage::Recovery), Error> {
    Store::open(Arc::clone(p) as Arc<dyn Platform>, Path::new(DIR), &opts())
}

fn contents(store: &Store) -> Model {
    store
        .read()
        .unwrap()
        .range(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn random_batch(rng: &SplitMix64, meta: u64) -> Batch {
    let n = 1 + rng.below(6) as usize;
    let ops = (0..n)
        .map(|_| {
            let k = format!("k{:02}", rng.below(40)).into_bytes();
            let v =
                (rng.below(4) != 0).then(|| vec![rng.next_u64() as u8; rng.below(300) as usize]);
            (k, v)
        })
        .collect();
    Batch { meta, ops }
}

fn apply(model: &mut Model, batch: &Batch) {
    for (k, v) in &batch.ops {
        match v {
            Some(v) => model.insert(k.clone(), v.clone()),
            None => model.remove(k),
        };
    }
}

fn wal_segments(p: &SimPlatform) -> usize {
    p.fs().list_dir(Path::new("/db/wal")).unwrap().len()
}

#[test]
fn replays_the_wal_after_a_crash() {
    let p = sim(1);
    let (store, rec) = open(&p).unwrap();
    assert_eq!(rec.replayed, 0);
    let rng = SplitMix64::new(1);
    let mut model = Model::new();
    for i in 1..=30 {
        let b = random_batch(&rng, i);
        store.commit(&b).unwrap();
        apply(&mut model, &b);
    }
    assert_eq!(contents(&store), model);
    drop(store);
    p.sim_fs().crash(); // no checkpoint since the start

    let (store, rec) = open(&p).unwrap();
    assert_eq!((rec.replayed, rec.meta), (30, 30));
    assert_eq!(contents(&store), model);
    // Recovery checkpointed, so the next open replays nothing.
    drop(store);
    p.sim_fs().crash();
    let (store, rec) = open(&p).unwrap();
    assert_eq!((rec.replayed, rec.meta), (0, 30));
    assert_eq!(contents(&store), model);
}

#[test]
fn unacknowledged_writes_may_vanish_acknowledged_ones_do_not() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    store
        .commit(&Batch {
            meta: 1,
            ops: vec![(b"a".to_vec(), Some(b"1".to_vec()))],
        })
        .unwrap();
    store
        .write(&Batch {
            meta: 2,
            ops: vec![(b"b".to_vec(), Some(b"2".to_vec()))],
        })
        .unwrap();
    assert_eq!(store.get(b"b").unwrap().as_deref(), Some(&b"2"[..])); // visible...
    drop(store);
    p.sim_fs().crash(); // ...but never synced
    let (store, rec) = open(&p).unwrap();
    assert_eq!(rec.meta, 1);
    assert_eq!(store.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(store.get(b"b").unwrap(), None);
}

#[test]
fn checkpoints_truncate_the_wal() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    let rng = SplitMix64::new(2);
    let mut model = Model::new();
    for i in 1..=60 {
        let b = random_batch(&rng, i);
        store.commit(&b).unwrap();
        apply(&mut model, &b);
    }
    let before = wal_segments(&p);
    assert!(before > 5, "segments: {before}");
    assert!(store.wal_since_checkpoint() > 0);
    store.checkpoint().unwrap();
    assert!(
        wal_segments(&p) <= 2,
        "segments after checkpoint: {}",
        wal_segments(&p)
    );
    assert_eq!(store.wal_since_checkpoint(), 0);
    drop(store);
    let (store, rec) = open(&p).unwrap();
    assert_eq!((rec.replayed, rec.meta), (0, 60));
    assert_eq!(contents(&store), model);
    store.pool().pager().check().unwrap();
    store.read().unwrap().check().unwrap();
}

#[test]
fn a_damaged_record_before_the_last_segment_is_corruption() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    let rng = SplitMix64::new(3);
    for i in 1..=40 {
        store.commit(&random_batch(&rng, i)).unwrap();
    }
    drop(store);
    let mut names = p.fs().list_dir(Path::new("/db/wal")).unwrap();
    names.sort();
    let first = Path::new("/db/wal").join(&names[0]);
    let f = p.fs().open(&first, OpenMode::ReadWrite).unwrap();
    f.write_all_at(&[0xFF; 4], 20).unwrap();
    f.sync().unwrap();
    assert!(matches!(open(&p), Err(Error::Corrupt { .. })));
}

#[test]
fn a_failed_fsync_poisons_the_store() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    p.sim_fs().set_faults(Faults {
        sync_error_ppm: 1_000_000,
        ..Faults::default()
    });
    let b = Batch {
        meta: 1,
        ops: vec![(b"a".to_vec(), Some(b"1".to_vec()))],
    };
    assert!(matches!(store.commit(&b), Err(Error::Io(_))));
    p.sim_fs().set_faults(Faults::default());
    assert!(matches!(store.commit(&b), Err(Error::Poisoned)));
    assert!(matches!(store.get(b"a"), Err(Error::Poisoned)));
}

#[test]
fn oversized_keys_are_refused_before_logging() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    let b = Batch {
        meta: 1,
        ops: vec![(vec![0; 5000], Some(vec![]))],
    };
    assert!(matches!(store.commit(&b), Err(Error::KeyTooLarge(5000))));
    store
        .commit(&Batch {
            meta: 2,
            ops: vec![(b"ok".to_vec(), Some(vec![]))],
        })
        .unwrap();
}

#[test]
fn concurrent_writers_and_readers() {
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    std::thread::scope(|s| {
        for t in 0..4u8 {
            let store = &store;
            s.spawn(move || {
                for i in 0..50u32 {
                    let k = [b'w', t, (i >> 8) as u8, i as u8].to_vec();
                    store
                        .commit(&Batch {
                            meta: 0,
                            ops: vec![(k.clone(), Some(vec![t]))],
                        })
                        .unwrap();
                    assert_eq!(store.get(&k).unwrap(), Some(vec![t]));
                }
            });
        }
    });
    assert_eq!(contents(&store).len(), 200);
}

#[test]
fn works_on_the_real_file_system() {
    let dir = std::env::temp_dir().join(format!("nucleus-store-{}", std::process::id()));
    let real: Arc<dyn Platform> = Arc::new(RealPlatform::new().unwrap());
    let (store, _) = Store::open(Arc::clone(&real), &dir, &opts()).unwrap();
    store
        .commit(&Batch {
            meta: 7,
            ops: vec![(b"k".to_vec(), Some(b"v".to_vec()))],
        })
        .unwrap();
    drop(store);
    let (store, rec) = Store::open(real, &dir, &opts()).unwrap();
    assert_eq!((rec.replayed, rec.meta), (1, 7));
    assert_eq!(store.get(b"k").unwrap(), Some(b"v".to_vec()));
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Batches with an occasional checkpoint.  Returns the models after each
/// batch attempted (index 0 = empty) and how many batches were acknowledged.
fn workload(p: &Arc<SimPlatform>, seed: u64) -> (Vec<Model>, usize) {
    let rng = SplitMix64::new(seed);
    let mut models = vec![Model::new()];
    let mut acked = 0;
    let _ = (|| -> Result<(), Error> {
        let (store, _) = open(p)?;
        let mut model = Model::new();
        for i in 1..=20u64 {
            let b = random_batch(&rng, i);
            apply(&mut model, &b);
            models.push(model.clone());
            // Like group commit: some batches are only written, and a later
            // wait makes all of them durable at once.  So unsynced batches
            // pile up across segment boundaries and into checkpoints.
            let lsn = store.write(&b)?;
            if rng.below(3) == 0 {
                store.wait_durable(lsn)?;
                acked = i as usize;
            }
            if i % 6 == 0 {
                store.checkpoint()?;
                acked = i as usize; // a checkpoint makes everything durable
            }
        }
        Ok(())
    })();
    (models, acked)
}

#[test]
fn crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("store_crash_at_every_io_point", 8, |seed| {
            let dry = sim(seed);
            let (models, acked) = workload(&dry, seed);
            assert!(acked >= 18);
            let total = dry.sim_fs().op_count();

            for n in 1..=total {
                let p = sim(seed);
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = workload(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                let ctx = format!("{mode:?} crash at op {n}");

                let (store, rec) = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let got = contents(&store);
                // Some prefix of the batches, at least every acknowledged one.
                let k = rec.meta as usize;
                assert!(
                    k >= acked && k < models.len(),
                    "{ctx}: recovered batch {k}, acked {acked}"
                );
                assert!(
                    got == models[k],
                    "{ctx}: state is not the one after batch {k}"
                );
                store
                    .read()
                    .unwrap()
                    .check()
                    .unwrap_or_else(|e| panic!("{ctx}: tree: {e}"));
                store
                    .pool()
                    .pager()
                    .check()
                    .unwrap_or_else(|e| panic!("{ctx}: pager: {e}"));
            }
        });
    }
}
