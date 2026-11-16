//! Engines behind the store (PLAN.md §M14.28 L1): a key range routed to
//! the trivial map engine — reads see one ordered map, a commit across
//! engines is atomic through a crash at any I/O point, branches keep
//! their own engines, the refusals.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::{Batch, EngineKind, Error, Store, StoreOptions};

const DIR: &str = "/db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn opts() -> StoreOptions {
    StoreOptions {
        cache_pages: 8,
        wal_segment_size: 2048,
        page_key: None,
        previous_page_key: None,
    }
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

/// Keys `k00` … `k39`; `k20` … `k29` are the map engine's.
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

/// Keys go to the engine their route names; reads — gets, scans either
/// way, a cursor — see one map; the routes survive a reopen.
#[test]
fn a_routed_range_reads_as_one_map() {
    let p = Arc::new(SimPlatform::new(1));
    let (store, _) = open(&p).unwrap();
    let id = store.add_engine(b"k20", b"k30", EngineKind::Map).unwrap();
    let rng = SplitMix64::new(1);
    let mut model = Model::new();
    for i in 1..=60 {
        let b = random_batch(&rng, i);
        store.commit(&b).unwrap();
        apply(&mut model, &b);
        if i % 20 == 0 {
            store.checkpoint().unwrap();
        }
    }
    assert_eq!(contents(&store), model);
    {
        let t = store.read().unwrap();
        // The routed keys are not in the B+tree.
        let in_tree: Vec<Vec<u8>> = t
            .tree()
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|e| e.unwrap().0)
            .collect();
        assert!(
            in_tree
                .iter()
                .all(|k| !(&k[..] >= b"k20" && &k[..] < b"k30"))
        );
        assert!(model.keys().any(|k| &k[..] >= b"k20" && &k[..] < b"k30"));
        for (k, v) in &model {
            assert_eq!(t.get(k).unwrap().as_ref(), Some(v));
        }
        let rev: Vec<Vec<u8>> = t
            .range_rev(Bound::Excluded(b"k05"), Bound::Included(b"k35"))
            .unwrap()
            .map(|e| e.unwrap().0)
            .collect();
        let want: Vec<Vec<u8>> = model
            .range::<[u8], _>((Bound::Excluded(&b"k05"[..]), Bound::Included(&b"k35"[..])))
            .rev()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(rev, want);
    }
    drop(store);
    p.sim_fs().crash();
    let (store, _) = open(&p).unwrap();
    assert_eq!(contents(&store), model);
    let routes = store.routes().unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!((routes[0].id, &routes[0].start[..]), (id, &b"k20"[..]));
}

/// The refusals: an empty range, one meeting another route, one holding
/// entries already.
#[test]
fn routes_are_refused_where_they_would_lose_or_mix_entries() {
    let p = Arc::new(SimPlatform::new(2));
    let (store, _) = open(&p).unwrap();
    store
        .commit(&Batch {
            meta: 1,
            ops: vec![(b"m".to_vec(), Some(b"1".to_vec()))],
        })
        .unwrap();
    let refused = |r: Result<u32, Error>| assert!(matches!(r, Err(Error::Engine(_))), "{r:?}");
    refused(store.add_engine(b"b", b"b", EngineKind::Map));
    refused(store.add_engine(b"l", b"n", EngineKind::Map));
    store.add_engine(b"a", b"c", EngineKind::Map).unwrap();
    refused(store.add_engine(b"b", b"d", EngineKind::Map));
    store.add_engine(b"c", b"d", EngineKind::Map).unwrap();
    assert_eq!(store.routes().unwrap().len(), 2);
}

/// A branch starts with a copy of its parent's engines and goes its own
/// way; its engines' files go with it when it is dropped; both survive a
/// crash.
#[test]
fn branches_keep_engines_of_their_own() {
    let p = Arc::new(SimPlatform::new(3));
    let (main, _) = open(&p).unwrap();
    main.add_engine(b"k20", b"k30", EngineKind::Map).unwrap();
    let put = |s: &Store, meta, k: &str, v: &str| {
        s.commit(&Batch {
            meta,
            ops: vec![(k.as_bytes().to_vec(), Some(v.as_bytes().to_vec()))],
        })
        .unwrap();
    };
    put(&main, 1, "k21", "main");
    put(&main, 2, "k01", "main");
    let dev = main.create_branch("dev").unwrap();
    put(&dev, 3, "k21", "dev");
    put(&dev, 4, "k22", "dev");
    put(&main, 3, "k23", "main");
    let get = |s: &Store, k: &str| {
        s.get(k.as_bytes())
            .unwrap()
            .map(|v| String::from_utf8(v).unwrap())
    };
    assert_eq!(get(&dev, "k21").as_deref(), Some("dev"));
    assert_eq!(get(&main, "k21").as_deref(), Some("main"));
    assert_eq!(get(&main, "k22"), None);
    assert_eq!(get(&dev, "k23"), None);
    // Where they differ: the routed range whole, as the engine keeps no
    // births to compare by.
    let diff = dev.diff_ranges(&main).unwrap();
    assert!(diff.is_none_or(|d| !d.is_empty()));
    main.checkpoint().unwrap();
    drop((main, dev));
    p.sim_fs().crash();
    let (main, _) = open(&p).unwrap();
    let dev = main.branch("dev").unwrap();
    assert_eq!(get(&dev, "k21").as_deref(), Some("dev"));
    assert_eq!(get(&dev, "k22").as_deref(), Some("dev"));
    assert_eq!(get(&main, "k23").as_deref(), Some("main"));
    assert_eq!(get(&main, "k01").as_deref(), Some("main"));
    drop(dev);
    main.drop_branch("dev").unwrap();
    let files: Vec<String> = p
        .fs()
        .list_dir(Path::new("/db/engines"))
        .unwrap()
        .into_iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    assert!(
        files.iter().all(|f| f == "routes" || f.starts_with("0-")),
        "{files:?}"
    );
}

/// Batches across the B+tree and the map engine, checkpoints between
/// them; a crash at every I/O point leaves a prefix of the batches —
/// every acknowledged one — in both engines alike: a commit across
/// engines is one WAL record, all or nothing.
fn workload(p: &Arc<SimPlatform>, seed: u64) -> (Vec<Model>, usize) {
    let rng = SplitMix64::new(seed);
    let mut models = vec![Model::new()];
    let mut acked = 0;
    let _ = (|| -> Result<(), Error> {
        let (store, _) = open(p)?;
        if store.routes()?.is_empty() {
            store.add_engine(b"k20", b"k30", EngineKind::Map)?;
        }
        let mut model = Model::new();
        for i in 1..=20u64 {
            let b = random_batch(&rng, i);
            apply(&mut model, &b);
            models.push(model.clone());
            let lsn = store.write(&b)?;
            if rng.below(3) == 0 {
                store.wait_durable(lsn)?;
                acked = i as usize;
            }
            if i % 6 == 0 {
                store.checkpoint()?;
                acked = i as usize;
            }
        }
        Ok(())
    })();
    (models, acked)
}

#[test]
fn commits_across_engines_survive_a_crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("engines_crash_at_every_io_point", 6, |seed| {
            let dry = Arc::new(SimPlatform::new(seed));
            let (models, acked) = workload(&dry, seed);
            assert!(acked >= 18);
            let total = dry.sim_fs().op_count();
            for n in 1..=total {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = workload(&p, seed);
                let ctx = format!("{mode:?} crash at op {n} of {total}");
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
            }
        });
    }
}
