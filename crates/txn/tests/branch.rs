//! Branches of a database of versioned keys (PLAN.md §M14.13 B2): an
//! engine per branch, its clock starting where its parent's was; commits
//! on each side invisible to the other; what transactions running at the
//! branch point wrote — spilled intents, unlogged fresh ranges — not in
//! the branch.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, MAIN_NAME};

fn opts() -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        txn_memory: 4096, // so a large transaction spills intents
        wal_minimal: true,
    }
}

fn open(p: &Arc<SimPlatform>) -> Engine {
    Engine::open(Arc::clone(p) as Arc<dyn Platform>, Path::new("/db"), opts()).unwrap()
}

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn all(e: &Engine) -> Model {
    e.begin()
        .scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn put(e: &Engine, k: &str, v: &str) {
    let mut t = e.begin();
    t.put(k.as_bytes(), v.as_bytes()).unwrap();
    t.commit().unwrap();
}

fn model(pairs: &[(&str, &str)]) -> Model {
    pairs
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

#[test]
fn a_branch_starts_from_its_parents_commits_and_goes_its_own_way() {
    let p = Arc::new(SimPlatform::new(1));
    let main = open(&p);
    put(&main, "a", "1");
    put(&main, "b", "1");
    let at = main.visible();
    let dev = main.create_branch("dev").unwrap();
    assert_eq!(dev.visible(), at);
    assert_eq!(dev.branch_info().name, "dev");
    assert_eq!(all(&dev), model(&[("a", "1"), ("b", "1")]));

    put(&main, "a", "main");
    put(&dev, "a", "dev");
    put(&dev, "c", "dev");
    assert_eq!(all(&main), model(&[("a", "main"), ("b", "1")]));
    assert_eq!(all(&dev), model(&[("a", "dev"), ("b", "1"), ("c", "dev")]));
    // Each clock goes on from the branch point on its own.
    assert!(main.visible() > at && dev.visible() > at);

    // One engine per branch, whoever asks.
    let again = main.branch("dev").unwrap();
    put(&again, "d", "dev");
    assert_eq!(all(&dev).len(), 4);
    let names: Vec<_> = main.branches().into_iter().map(|b| b.name).collect();
    assert_eq!(names, [MAIN_NAME, "dev"]);

    // Reopened: both, from their own logs.
    let (m, d) = (all(&main), all(&dev));
    drop((main, dev, again));
    p.sim_fs().crash();
    let main = open(&p);
    let dev = main.branch("dev").unwrap();
    assert_eq!(all(&main), m);
    assert_eq!(all(&dev), d);
    assert_eq!(dev.branch(MAIN_NAME).unwrap().visible(), main.visible());
    main.store().check_pages().unwrap();
}

#[test]
fn transactions_running_at_the_branch_point_are_not_in_the_branch() {
    let p = Arc::new(SimPlatform::new(2));
    let main = open(&p);
    put(&main, "k", "before");
    // A transaction large enough to spill intents into the store, and one
    // writing a fresh range without the WAL — both running.
    let mut big = main.begin();
    for i in 0..400 {
        big.put(format!("big{i:04}").as_bytes(), &[7; 64]).unwrap();
    }
    let mut fresh = main.begin();
    fresh.fresh(b"f", b"g");
    for i in 0..400 {
        fresh.put(format!("f{i:04}").as_bytes(), &[8; 64]).unwrap();
    }
    assert!(main.intent_count().unwrap() > 0);

    let dev = main.create_branch("dev").unwrap();
    assert_eq!(all(&dev), model(&[("k", "before")]));
    // The spilled intents are there, as after a crash: no one's, and
    // vacuum deletes them.
    assert!(dev.intent_count().unwrap() > 0);
    dev.vacuum().unwrap();
    assert_eq!(dev.intent_count().unwrap(), 0);
    assert!(main.intent_count().unwrap() > 0);
    // The branch writes the same keys freely.
    let mut t = dev.begin();
    t.put(b"big0000", b"dev").unwrap();
    t.put(b"f0000", b"dev").unwrap();
    t.commit().unwrap();

    big.commit().unwrap();
    fresh.commit().unwrap();
    assert_eq!(all(&main).len(), 801);
    assert_eq!(
        all(&dev),
        model(&[("big0000", "dev"), ("f0000", "dev"), ("k", "before")])
    );
    main.vacuum().unwrap();
    dev.vacuum().unwrap();
    main.store().check_pages().unwrap();
}

#[test]
fn dropped_and_reset_branches() {
    let p = Arc::new(SimPlatform::new(3));
    let main = open(&p);
    put(&main, "a", "1");
    let dev = main.create_branch("dev").unwrap();
    put(&dev, "x", "1");
    let t = dev.begin();
    main.drop_branch("dev").unwrap();
    assert!(t.get(b"a").is_err());
    let mut w = dev.begin();
    assert!(
        w.put(b"y", b"1")
            .and_then(|_| w.commit().map(|_| ()))
            .is_err()
    );
    assert!(matches!(main.branch("dev"), Err(Error::Storage(_))));

    let dev = main.create_branch("dev").unwrap();
    put(&dev, "x", "2");
    put(&main, "a", "2");
    let dev2 = main.reset_branch("dev").unwrap();
    assert_eq!(all(&dev2), model(&[("a", "2")]));
    assert!(dev.begin().get(b"x").is_err());
    put(&dev2, "z", "3");
    assert_eq!(
        all(&main.branch("dev").unwrap()),
        model(&[("a", "2"), ("z", "3")])
    );
    main.store().check_pages().unwrap();
}
