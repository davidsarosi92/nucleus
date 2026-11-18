//! Store tests: WAL replay, checkpoints, torn logs, poisoning, and a crash
//! at every I/O point of a workload of batches and checkpoints — the odd
//! seeds' with the data file encrypted (PLAN.md §M14.30 K2b).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::real::RealPlatform;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{OpenMode, Platform, Rng, SplitMix64};
use nucleus_storage::{Batch, Error, PageCipher, Store, StoreOptions, Wal};

const DIR: &str = "/db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

thread_local! {
    /// Whether this thread's stores are encrypted ([`encrypt_if_odd`]).
    static ENCRYPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The odd seeds of a crash test run with the data file encrypted —
/// until the guard returned is dropped, so a test after it (all on one
/// thread where there are no threads, as under WASI) starts in the clear.
#[must_use]
fn encrypt_if_odd(seed: u64) -> InTheClearAfter {
    ENCRYPTED.set(seed % 2 == 1);
    InTheClearAfter
}

/// Sets this thread's stores in the clear again when dropped.
struct InTheClearAfter;

impl Drop for InTheClearAfter {
    fn drop(&mut self) {
        ENCRYPTED.set(false);
    }
}

fn opts() -> StoreOptions {
    StoreOptions {
        cache_pages: 8,
        wal_segment_size: 2048,
        page_key: ENCRYPTED.get().then(|| PageCipher::new([7; 32])),
        previous_page_key: None,
        compaction_bytes_per_sec: 0,
        lsm_cache_bytes: nucleus_storage::lsm::BLOCK_CACHE_BYTES,
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

fn wal_files(p: &SimPlatform, suffix: &str) -> Vec<std::ffi::OsString> {
    let mut names = p.fs().list_dir(Path::new("/db/wal")).unwrap();
    names.retain(|n| n.to_string_lossy().ends_with(suffix));
    names
}

fn wal_segments(p: &SimPlatform) -> usize {
    wal_files(p, ".wal").len()
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

/// Where the file system writes in place, segments are grown ahead by
/// zeros and the ones a checkpoint drops are kept and reused (§M14.28 L0);
/// on a copy-on-write one they grow as written and are deleted.
#[test]
fn dropped_segments_are_recycled_where_files_are_written_in_place() {
    for in_place in [true, false] {
        let p = sim(1);
        p.sim_fs().set_writes_in_place(in_place);
        let (store, _) = open(&p).unwrap();
        let rng = SplitMix64::new(2);
        let mut model = Model::new();
        let mut meta = 0;
        for round in 0..4 {
            for _ in 0..60 {
                meta += 1;
                let b = random_batch(&rng, meta);
                store.commit(&b).unwrap();
                apply(&mut model, &b);
            }
            store.checkpoint().unwrap();
            let spares = wal_files(&p, ".spare").len();
            if in_place {
                assert!(spares > 2, "round {round}: spares {spares}");
            } else {
                assert_eq!(spares, 0);
            }
        }
        // The current segment grown ahead of its records (to its full
        // length, segments being small here).
        let names = wal_files(&p, ".wal");
        let last = Path::new("/db/wal").join(names.last().unwrap());
        let len = p
            .fs()
            .open(&last, OpenMode::ReadOnly)
            .unwrap()
            .size()
            .unwrap();
        if in_place {
            assert_eq!(len, opts().wal_segment_size, "preallocated");
        }
        drop(store);
        let (store, rec) = open(&p).unwrap();
        assert_eq!(rec.meta, meta);
        assert_eq!(contents(&store), model);
        // Reused again after reopening.
        for _ in 0..60 {
            meta += 1;
            let b = random_batch(&rng, meta);
            store.commit(&b).unwrap();
            apply(&mut model, &b);
        }
        drop(store);
        let (store, rec) = open(&p).unwrap();
        assert_eq!(rec.meta, meta);
        assert_eq!(contents(&store), model);
    }
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
    let mut names = wal_files(&p, ".wal");
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
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
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
    let dir = nucleus_platform::temp_dir()
        .join(format!("nucleus-store-{}", nucleus_platform::process_id()));
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

/// Batches with an occasional checkpoint (one a scrub).  Returns the models after each
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
            // Now and then its last put deferred (a key of its own).
            let lsn = match b.ops.split_last() {
                Some(((k, Some(v)), rest))
                    if rng.below(2) == 0 && rest.iter().all(|(r, _)| r != k) =>
                {
                    let head = Batch {
                        meta: b.meta,
                        ops: rest.to_vec(),
                    };
                    store.write_deferring(&head, vec![(k.clone(), v.clone())], &Keep)?
                }
                _ => store.write(&b)?,
            };
            if rng.below(3) == 0 {
                store.wait_durable(lsn)?;
                acked = i as usize;
            }
            if i % 6 == 0 {
                // One of them a scrub: its checkpoints, the log moved on,
                // segments and free pages zeroed.
                if i == 12 {
                    store.scrub()?;
                } else {
                    store.checkpoint()?;
                }
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
            let _encrypted = encrypt_if_odd(seed);
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

/// Batches with checkpoints whose second half runs after more batches
/// (§M14.20 W4): writes go on between [`Store::checkpoint_begin`] and
/// [`Checkpoint::finish`](nucleus_storage::Checkpoint::finish), copying the
/// frozen pages they change.  Returns the models after each batch and how
/// many were acknowledged.
fn workload_overlapping(p: &Arc<SimPlatform>, seed: u64) -> (Vec<Model>, usize) {
    let rng = SplitMix64::new(seed);
    let mut models = vec![Model::new()];
    let mut acked = 0;
    let _ = (|| -> Result<(), Error> {
        let (store, _) = open(p)?;
        let mut model = Model::new();
        let mut running = None;
        for i in 1..=24u64 {
            let b = random_batch(&rng, i);
            apply(&mut model, &b);
            models.push(model.clone());
            let lsn = store.write(&b)?;
            if rng.below(3) == 0 {
                store.wait_durable(lsn)?;
                acked = acked.max(i as usize);
            }
            if i % 5 == 0 && running.is_none() {
                // Frozen here: this batch and those before it.
                running = Some((store.checkpoint_begin()?, i as usize));
            } else if i % 5 == 2
                && let Some((c, upto)) = running.take()
            {
                c.finish()?;
                acked = acked.max(upto); // the checkpoint made them durable
            }
        }
        if let Some((c, upto)) = running.take() {
            c.finish()?;
            acked = acked.max(upto);
        }
        Ok(())
    })();
    (models, acked)
}

#[test]
fn checkpoints_overlapping_writes() {
    // Without a crash: everything there, the pager's accounts exact.
    let p = sim(7);
    let (models, acked) = workload_overlapping(&p, 7);
    assert!(acked >= 20, "acked {acked}");
    let (store, rec) = open(&p).unwrap();
    let k = rec.meta as usize;
    assert!(k >= acked);
    assert_eq!(contents(&store), models[k]);
    store.pool().pager().check().unwrap();
    store.read().unwrap().check().unwrap();
}

/// The files under `dir` that hold `needle`, raw.
fn files_holding(p: &SimPlatform, dir: &Path, needle: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut names = p.fs().list_dir(dir).unwrap();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        if p.fs().list_dir(&path).is_ok() {
            out.extend(files_holding(p, &path, needle));
            continue;
        }
        let file = p.fs().open(&path, OpenMode::ReadOnly).unwrap();
        let mut bytes = vec![0u8; file.size().unwrap() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        if bytes.windows(needle.len()).any(|w| w == needle) {
            out.push(name.to_string_lossy().into_owned());
        }
    }
    out
}

/// An encrypted store (PLAN.md §M14.30 K2b, K2c): a value planted in it
/// — checkpointed into the data file, and written since, in the log only
/// — is found in a store in the clear, in both, and in no file of an
/// encrypted one, which recovers its log and reopens under its key only.
/// A log in the clear opened with a key is refused, not cut off.
#[test]
fn a_planted_value_is_in_no_file_of_an_encrypted_store() {
    let planted = b"planted-value-4cb1e0f2-searched-for".to_vec();
    for encrypted in [false, true] {
        ENCRYPTED.set(encrypted);
        let p = sim(3);
        {
            let (store, _) = open(&p).unwrap();
            for i in 0..60u64 {
                let mut v = planted.clone();
                v.extend_from_slice(&i.to_le_bytes());
                let lsn = store
                    .write(&Batch {
                        meta: i + 1,
                        ops: vec![(format!("k{i:03}").into_bytes(), Some(v))],
                    })
                    .unwrap();
                store.wait_durable(lsn).unwrap();
                if i == 49 {
                    store.checkpoint().unwrap();
                }
            }
        }
        let found = files_holding(&p, Path::new(DIR), &planted);
        if encrypted {
            assert!(found.is_empty(), "found in {found:?}");
        } else {
            assert!(found.iter().any(|f| f == "data"), "{found:?}");
            assert!(found.iter().any(|f| f.ends_with(".wal")), "{found:?}");
        }

        let (store, rec) = open(&p).unwrap();
        assert_eq!(rec.meta, 60);
        assert_eq!(rec.replayed, 10);
        assert_eq!(contents(&store).len(), 60);
        drop(store);
        ENCRYPTED.set(!encrypted);
        let refused = open(&p).err().map(|e| e.to_string());
        assert!(
            matches!(&refused, Some(e) if e.contains("encrypted") || e.contains("key")),
            "{refused:?}"
        );
        if !encrypted {
            // The log alone, in the clear, opened with a key.
            let key = Some(PageCipher::new([7; 32]));
            let p2: Arc<dyn Platform> = Arc::clone(&p) as Arc<dyn Platform>;
            match Wal::open_with(p2, Path::new("/db/wal"), 2048, 0, key) {
                Err(Error::Key(_)) => {}
                Err(e) => panic!("{e}"),
                Ok(_) => panic!("a log in the clear opened as encrypted"),
            }
        }
    }
    let p = sim(3);
    ENCRYPTED.set(true);
    drop(open(&p).unwrap());
    let other = StoreOptions {
        page_key: Some(PageCipher::new([8; 32])),
        ..opts()
    };
    let Err(e) = Store::open(Arc::clone(&p) as Arc<dyn Platform>, Path::new(DIR), &other) else {
        panic!("another key accepted");
    };
    assert!(e.to_string().contains("another key"), "{e}");
    ENCRYPTED.set(false);
}

/// A store keeps the cipher it was made with (PLAN.md §M14.30, the speed
/// after K2): made with each of the two, its pages and its log — the last
/// writes only logged — are read with that cipher when it is opened with
/// the key alone, whichever this CPU would choose for a new one; and the
/// planted value is in no file either way.
#[test]
fn a_store_keeps_its_cipher() {
    use nucleus_storage::cipher::{CIPHER_AES256_GCM_SIV, CIPHER_CHACHA20_SIV};
    let planted = b"cipher-kept-planted-71e3a9d0".to_vec();
    for code in [CIPHER_CHACHA20_SIV, CIPHER_AES256_GCM_SIV] {
        let p = sim(4);
        let made = StoreOptions {
            page_key: Some(PageCipher::new([7; 32]).for_code(code).unwrap()),
            ..opts()
        };
        {
            let (store, _) =
                Store::open(Arc::clone(&p) as Arc<dyn Platform>, Path::new(DIR), &made).unwrap();
            for i in 0..40u64 {
                let mut v = planted.clone();
                v.extend_from_slice(&i.to_le_bytes());
                let lsn = store
                    .write(&Batch {
                        meta: i + 1,
                        ops: vec![(format!("k{i:03}").into_bytes(), Some(v))],
                    })
                    .unwrap();
                store.wait_durable(lsn).unwrap();
                if i == 29 {
                    store.checkpoint().unwrap();
                }
            }
        }
        assert!(files_holding(&p, Path::new(DIR), &planted).is_empty());
        let fs = p.fs();
        assert_eq!(
            nucleus_storage::pager::file_cipher(fs, &Path::new(DIR).join("data")).unwrap(),
            Some(code)
        );
        ENCRYPTED.set(true);
        let (store, rec) = open(&p).unwrap();
        ENCRYPTED.set(false);
        assert_eq!((rec.meta, rec.replayed), (40, 10), "cipher {code}");
        assert_eq!(contents(&store).len(), 40);
    }
}

/// A store in the clear — its main tree and a branch, the last writes
/// only logged — built for [`a_store_encrypted_and_a_crash_at_every_point`];
/// its contents, main and branch.
fn clear_store_for_encryption(p: &Arc<SimPlatform>) -> (Model, Model) {
    ENCRYPTED.set(false);
    let (store, _) = open(p).unwrap();
    // Some keys an LSM tree's, some a map engine's: their files are
    // sealed too (§M14.28 L4e).
    store
        .add_engine(b"k010", b"k020", nucleus_storage::EngineKind::Lsm)
        .unwrap();
    store
        .add_engine(b"k024", b"k028", nucleus_storage::EngineKind::Map)
        .unwrap();
    let planted = b"encrypt-me-planted-9d2c41e0";
    let put = |s: &Store, i: u64| {
        let mut v = planted.to_vec();
        v.extend_from_slice(&i.to_le_bytes());
        s.commit(&Batch {
            meta: i,
            ops: vec![(format!("k{i:03}").into_bytes(), Some(v))],
        })
        .unwrap();
    };
    for i in 1..=30 {
        put(&store, i);
        if i == 20 {
            store.checkpoint().unwrap();
        }
    }
    let branch = store.create_branch("b").unwrap();
    for i in 31..=36 {
        put(&branch, i);
    }
    put(&store, 37);
    (contents(&store), contents(&branch))
}

/// A store encrypted in place (PLAN.md §M14.30 K2e), with a crash at
/// every I/O point of it: what opens afterwards is the store in the clear
/// or encrypted — never neither — with everything in it, main and branch;
/// encrypting again finishes it.  Encrypted, the planted value is in no
/// file — the engines' files sealed too —, and the store does not open
/// without its key.
#[test]
fn a_store_encrypted_and_a_crash_at_every_point() {
    // The key `opts` gives an encrypted store.
    let key = PageCipher::new([7; 32]);
    let planted = b"encrypt-me-planted-9d2c41e0";
    let dir = Path::new(DIR);
    let check = |p: &Arc<SimPlatform>, want: &(Model, Model), ctx: &str| {
        let cipher = nucleus_storage::pager::file_cipher(p.fs(), &dir.join("data")).unwrap();
        ENCRYPTED.set(cipher != Some(0));
        let (store, _) = open(p).unwrap_or_else(|e| panic!("{ctx}: {e}"));
        assert!(contents(&store) == want.0, "{ctx}: main");
        assert!(
            contents(&store.branch("b").unwrap()) == want.1,
            "{ctx}: branch"
        );
        store.pool().pager().check().unwrap();
        cipher != Some(0)
    };

    let dry = sim(21);
    let want = clear_store_for_encryption(&dry);
    assert!(!files_holding(&dry, dir, planted).is_empty());
    assert!(
        !files_holding(&dry, &dir.join("engines"), planted).is_empty(),
        "the engines' files hold it in the clear"
    );
    let before = dry.sim_fs().op_count();
    let platform: Arc<dyn Platform> = Arc::clone(&dry) as Arc<dyn Platform>;
    let pages = Store::encrypt(Arc::clone(&platform), dir, &opts(), &key).unwrap();
    assert!(pages > 4, "{pages} pages");
    let total = dry.sim_fs().op_count() - before;
    assert!(check(&dry, &want, "after"));
    assert!(
        files_holding(&dry, dir, planted).is_empty(),
        "{:?}",
        files_holding(&dry, dir, planted)
    );
    ENCRYPTED.set(false);
    assert!(open(&dry).is_err(), "opened without its key");
    assert!(matches!(
        Store::encrypt(platform, dir, &opts(), &key),
        Err(Error::Key(_))
    ));

    for n in 1..=total {
        let p = sim(21);
        clear_store_for_encryption(&p);
        p.sim_fs().crash_after_ops(n);
        let platform: Arc<dyn Platform> = Arc::clone(&p) as Arc<dyn Platform>;
        assert!(Store::encrypt(Arc::clone(&platform), dir, &opts(), &key).is_err());
        let ctx = format!("crash at op {n} of {total}");
        if !check(&p, &want, &ctx) {
            Store::encrypt(platform, dir, &opts(), &key)
                .unwrap_or_else(|e| panic!("{ctx}: again: {e}"));
            assert!(check(&p, &want, &ctx), "{ctx}: encrypted again");
        }
    }
    ENCRYPTED.set(false);
}

#[test]
fn crash_at_every_io_point_of_overlapping_checkpoints() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("store_crash_overlapping_checkpoints", 8, |seed| {
            let _encrypted = encrypt_if_odd(seed);
            let dry = sim(seed);
            let (models, _) = workload_overlapping(&dry, seed);
            let total = dry.sim_fs().op_count();
            for n in 1..=total {
                let p = sim(seed);
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = workload_overlapping(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                let ctx = format!("{mode:?} crash at op {n}");
                let (store, rec) = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let k = rec.meta as usize;
                assert!(
                    k >= acked && k < models.len(),
                    "{ctx}: recovered batch {k}, acked {acked}"
                );
                assert!(
                    contents(&store) == models[k],
                    "{ctx}: not the state after batch {k}"
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

/// Writers and a checkpointer at once, on real files (§M14.20 W4): every
/// checkpoint's slow half runs while writers copy its pages, evict them,
/// free them and grow the file.  Then the store, reopened, holds every
/// write.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn checkpoints_while_writing_on_real_files() {
    let dir = nucleus_platform::temp_dir()
        .join(format!("nucleus-ckpt-{}", nucleus_platform::process_id()));
    let real: Arc<dyn Platform> = Arc::new(RealPlatform::new().unwrap());
    let o = StoreOptions {
        cache_pages: 16,
        wal_segment_size: 64 << 10,
        ..Default::default()
    };
    let (store, _) = Store::open(Arc::clone(&real), &dir, &o).unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let checkpoints = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|s| {
        let writers: Vec<_> = (0..4u8)
            .map(|t| {
                let store = &store;
                s.spawn(move || {
                    let rng = SplitMix64::new(u64::from(t) + 1);
                    let mut model = Model::new();
                    for i in 0..400u32 {
                        let ops: Vec<_> = (0..1 + rng.below(8))
                            .map(|_| {
                                let k = format!("{t}-{:03}", rng.below(300)).into_bytes();
                                let v = (rng.below(5) != 0)
                                    .then(|| vec![i as u8; rng.below(600) as usize]);
                                (k, v)
                            })
                            .collect();
                        let b = Batch { meta: 0, ops };
                        store.commit(&b).unwrap();
                        apply(&mut model, &b);
                    }
                    model
                })
            })
            .collect();
        s.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                store.checkpoint().unwrap();
                checkpoints.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let mut model = Model::new();
        for w in writers {
            model.extend(w.join().unwrap());
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(contents(&store), model);
        store.read().unwrap().check().unwrap();
    });
    assert!(checkpoints.load(std::sync::atomic::Ordering::Relaxed) > 2);
    let model = contents(&store);
    store.pool().pager().check().unwrap();
    drop(store);
    let (store, _) = Store::open(real, &dir, &o).unwrap();
    assert_eq!(contents(&store), model);
    store.pool().pager().check().unwrap();
    store.read().unwrap().check().unwrap();
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What [`Store::scrub`] is for (PLAN.md §M14.24 R2): entries deleted —
/// inline values, compressed ones, overflow chains, keys — leave bytes in
/// free pages and the WAL after an ordinary checkpoint; after a scrub no
/// file holds them, and the store reopens with everything else.
#[test]
fn a_scrub_leaves_no_deleted_bytes_in_any_file() {
    for in_place in [true, false] {
        let p = sim(3);
        p.sim_fs().set_writes_in_place(in_place);
        let (store, _) = open(&p).unwrap();
        let secret = |i: u32| format!("SECRET-{i:04}-erase-me").into_bytes();
        let mut model = Model::new();
        let mut meta = 0;
        let mut put = |store: &Store, model: &mut Model, k: Vec<u8>, v: Vec<u8>| {
            meta += 1;
            let b = Batch {
                meta,
                ops: vec![(k.clone(), Some(v.clone()))],
            };
            store.commit(&b).unwrap();
            model.insert(k, v);
        };
        for i in 0..40u32 {
            // Inline; compressed (1 KiB and more, repetitive); in an
            // overflow chain (past a page); and in a key.
            let s = secret(i);
            put(
                &store,
                &mut model,
                format!("a{i:03}").into_bytes(),
                s.clone(),
            );
            put(
                &store,
                &mut model,
                format!("b{i:03}").into_bytes(),
                s.repeat(60),
            );
            let mut big = s.clone();
            big.extend((0..9000u32).map(|x| (x.wrapping_mul(2654435761) >> 13) as u8));
            put(&store, &mut model, format!("c{i:03}").into_bytes(), big);
            let mut key = b"d".to_vec();
            key.extend_from_slice(&s);
            put(&store, &mut model, key, b"v".to_vec());
            // Kept: neighbours sharing their pages.
            put(
                &store,
                &mut model,
                format!("e{i:03}").into_bytes(),
                b"kept".repeat(20),
            );
        }
        store.checkpoint().unwrap();
        let doomed: Vec<Vec<u8>> = model
            .keys()
            .filter(|k| !k.starts_with(b"e"))
            .cloned()
            .collect();
        meta += 1;
        let b = Batch {
            meta,
            ops: doomed.iter().map(|k| (k.clone(), None)).collect(),
        };
        store.commit(&b).unwrap();
        for k in &doomed {
            model.remove(k);
        }
        store.checkpoint().unwrap();
        let needles: Vec<Vec<u8>> = (0..40).map(secret).collect();
        let found = |p: &SimPlatform| {
            nucleus_storage::scan::find(p.fs(), Path::new(DIR), &needles)
                .unwrap()
                .len()
        };
        assert!(found(&p) > 40, "the deleted bytes, before");
        let done = store.scrub().unwrap();
        assert!(done.pages > 0);
        assert_eq!(found(&p), 0, "in place: {in_place}");
        // Everything else is there, and stays after reopening.
        assert_eq!(contents(&store), model);
        drop(store);
        let (store, _) = open(&p).unwrap();
        assert_eq!(contents(&store), model);
        assert_eq!(found(&p), 0);
    }
}

/// The search reads the data file as the tree wrote it: a value whose
/// bytes compression split, or a key whose leaf keeps its prefix apart,
/// is still found — though a plain search of the file does not find it.
#[test]
fn the_search_decodes_leaves() {
    let p = sim(4);
    let (store, _) = open(&p).unwrap();
    // Repetitive, so compression keeps it as a literal and a match.
    let packed_needle = b"zyxw".repeat(6);
    let mut packed = packed_needle.clone();
    packed.extend((0..2048u32).map(|x| b"abcdefgh"[(x % 8) as usize]));
    // Keys sharing a prefix (a leaf stores it once, the rests apart).
    let key_needle = b"KEYNEEDLE-1".to_vec();
    let ops = vec![
        (b"KEYNEEDLE-1".to_vec(), Some(b"one".to_vec())),
        (b"KEYNEEDLE-2".to_vec(), Some(b"two".to_vec())),
        (b"KEYNEEDLE-3".to_vec(), Some(packed)),
    ];
    store.commit(&Batch { meta: 1, ops }).unwrap();
    store.checkpoint().unwrap();
    let data = Path::new(DIR).join("data");
    let mut raw = Vec::new();
    let f = p.fs().open(&data, OpenMode::ReadOnly).unwrap();
    raw.resize(f.size().unwrap() as usize, 0);
    f.read_exact_at(&mut raw, 0).unwrap();
    for needle in [packed_needle, key_needle] {
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "not in the file as it is: {}",
            String::from_utf8_lossy(&needle)
        );
        let found = nucleus_storage::scan::find(p.fs(), Path::new(DIR), &[needle])
            .unwrap()
            .into_iter()
            .filter(|f| f.file == data)
            .count();
        assert_eq!(found, 1, "found in the data file's leaf");
    }
}

/// Drops every entry of the leaves a batch rewrites whose key starts
/// with `x` — without logging it.
struct DropX;

impl nucleus_storage::btree::LeafFilter for DropX {
    fn drops(&self, entries: &[nucleus_storage::btree::LeafEntry], drop: &mut [bool]) {
        for (e, d) in entries.iter().zip(drop.iter_mut()) {
            *d = e.key.starts_with(b"x");
        }
    }
}

#[test]
fn replay_deletes_a_covered_range_whole() {
    // A filter drops entries without logging them; a later delete of what
    // was left there, covering the range, takes them on replay too.
    let p = sim(1);
    let (store, _) = open(&p).unwrap();
    let put = |k: &[u8]| (k.to_vec(), Some(b"v".to_vec()));
    store
        .commit(&Batch {
            meta: 1,
            ops: vec![put(b"x1"), put(b"x2"), put(b"x3")],
        })
        .unwrap();
    // A write into the same leaf, filtered: x1 and x2 go, unlogged — the
    // batch's own x3 too, as it lands in the rewritten leaf.
    let lsn = store
        .write_filtered(
            &Batch {
                meta: 2,
                ops: vec![put(b"y")],
            },
            &DropX,
        )
        .unwrap();
    store.wait_durable(lsn).unwrap();
    assert_eq!(contents(&store).keys().collect::<Vec<_>>(), [b"y"]);
    // Nothing of x is left to delete; the range is covered.
    store
        .write_covering(
            &Batch {
                meta: 3,
                ops: vec![(b"y".to_vec(), None)],
            },
            &[(b"x".to_vec(), b"y".to_vec())],
        )
        .unwrap();
    store
        .commit(&Batch {
            meta: 4,
            ops: vec![put(b"z")],
        })
        .unwrap();
    drop(store);
    p.sim_fs().crash();
    let (store, rec) = open(&p).unwrap();
    assert_eq!(rec.meta, 4);
    // Replay put x1–x3 back, unfiltered, and the covering delete took them.
    assert_eq!(contents(&store).keys().collect::<Vec<_>>(), [b"z"]);
}

/// A filter that drops nothing.
struct Keep;

impl nucleus_storage::btree::LeafFilter for Keep {
    fn drops(&self, _: &[nucleus_storage::btree::LeafEntry], _: &mut [bool]) {}
}

/// The tree with the deferred puts over it.
fn contents_deferred(store: &Store) -> Model {
    let tree = store.read().unwrap();
    let mut m: Model = tree
        .range(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    m.extend(store.deferred().iter().map(|(k, v)| (k.clone(), v.clone())));
    m
}

#[test]
fn deferred_puts_wait_in_memory_then_reach_the_tree() {
    let p = sim(2);
    let (store, _) = open(&p).unwrap();
    let put = |k: &[u8]| (k.to_vec(), Some(b"v".to_vec()));
    let lsn = store
        .write_deferring(
            &Batch {
                meta: 1,
                ops: vec![put(b"a")],
            },
            vec![
                (b"t1".to_vec(), b"1".to_vec()),
                (b"t2".to_vec(), b"2".to_vec()),
            ],
            &Keep,
        )
        .unwrap();
    store.wait_durable(lsn).unwrap();
    // Not in the tree: beside it.
    assert_eq!(contents(&store).keys().collect::<Vec<_>>(), [b"a"]);
    assert_eq!(store.deferred().len(), 2);
    // A later batch's delete of one replaces it.
    store
        .commit(&Batch {
            meta: 2,
            ops: vec![(b"t1".to_vec(), None)],
        })
        .unwrap();
    assert_eq!(
        contents_deferred(&store).keys().collect::<Vec<_>>(),
        [&b"a"[..], b"t2"]
    );
    // A crash: replay puts them back — the delete after them too.
    drop(store);
    p.sim_fs().crash();
    let (store, rec) = open(&p).unwrap();
    assert_eq!(rec.meta, 2);
    // (Open checkpoints after a replay: into the tree.)
    assert!(store.deferred().is_empty());
    assert_eq!(
        contents(&store).keys().collect::<Vec<_>>(),
        [&b"a"[..], b"t2"]
    );
    // A checkpoint takes them into the tree.
    store
        .write_deferring(
            &Batch {
                meta: 3,
                ops: vec![],
            },
            vec![(b"t3".to_vec(), b"3".to_vec())],
            &Keep,
        )
        .unwrap();
    assert_eq!(store.deferred().len(), 1);
    store.checkpoint().unwrap();
    assert!(store.deferred().is_empty());
    assert_eq!(contents(&store).len(), 3);
    // Past DEFERRED_MAX they go into the tree with the batch.
    let mut model = contents(&store);
    for i in 0..nucleus_storage::DEFERRED_MAX as u64 + 10 {
        let k = format!("u{i:06}").into_bytes();
        store
            .write_deferring(
                &Batch {
                    meta: 4 + i,
                    ops: vec![],
                },
                vec![(k.clone(), b"x".to_vec())],
                &Keep,
            )
            .unwrap();
        model.insert(k, b"x".to_vec());
        assert!(store.deferred().len() <= nucleus_storage::DEFERRED_MAX);
    }
    assert_eq!(contents_deferred(&store), model);
    store.read().unwrap().check().unwrap();
}
