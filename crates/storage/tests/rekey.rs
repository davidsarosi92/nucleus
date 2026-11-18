//! A new key for an encrypted store, online (PLAN.md §M14.30 K3f): the
//! pages the old key sealed sealed again in place while writes go on, a
//! store reopened midway, a store given its new key as it opens, and a
//! crash at every I/O point of it — torn writes included.  The store has
//! an LSM tree's range and a map engine's, whose files are sealed again
//! too (§M14.28 L4e).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::Platform;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_storage::{Batch, EngineKind, PageCipher, Rekey, Store, StoreOptions};

const DIR: &str = "/db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn old_key() -> PageCipher {
    PageCipher::new([7; 32])
}

fn new_key() -> PageCipher {
    PageCipher::new([9; 32])
}

fn opts(key: PageCipher, previous: Option<PageCipher>) -> StoreOptions {
    StoreOptions {
        cache_pages: 16,
        wal_segment_size: 1 << 14,
        page_key: Some(key),
        previous_page_key: previous,
        compaction_bytes_per_sec: 0,
        lsm_cache_bytes: nucleus_storage::lsm::BLOCK_CACHE_BYTES,
    }
}

fn open(p: &Arc<SimPlatform>, o: &StoreOptions) -> nucleus_storage::Result<Store> {
    Store::open(Arc::clone(p) as Arc<dyn Platform>, Path::new(DIR), o).map(|(s, _)| s)
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

/// Batch `i`: a few keys put (values of a few hundred bytes, so the tree
/// spans pages) and one deleted.
fn batch(i: u64) -> Batch {
    let mut ops = Vec::new();
    for j in 0..4 {
        let k = (i * 7 + j * 13) % 300;
        ops.push((
            format!("k{k:04}").into_bytes(),
            Some(vec![(i + j) as u8; 200 + (k as usize % 300)]),
        ));
    }
    ops.push((format!("k{:04}", (i * 11) % 300).into_bytes(), None));
    Batch { meta: i, ops }
}

fn apply(m: &mut Model, b: &Batch) {
    for (k, v) in &b.ops {
        match v {
            Some(v) => m.insert(k.clone(), v.clone()),
            None => m.remove(k),
        };
    }
}

/// A store under the old key: 120 batches, a branch made at the 60th,
/// checkpoints along the way, the last batches only logged.  The main
/// tree's contents and the branch's.
fn store_under_the_old_key(p: &Arc<SimPlatform>) -> (Model, Model) {
    let store = open(p, &opts(old_key(), None)).unwrap();
    store
        .add_engine(b"k0100", b"k0180", EngineKind::Lsm)
        .unwrap();
    store
        .add_engine(b"k0200", b"k0240", EngineKind::Map)
        .unwrap();
    let (mut main, mut branch) = (Model::new(), Model::new());
    for i in 1..=120 {
        let b = batch(i);
        store.wait_durable(store.write(&b).unwrap()).unwrap();
        apply(&mut main, &b);
        if i == 60 {
            store.create_branch("b").unwrap();
            branch = main.clone();
        }
        if i % 25 == 0 {
            store.checkpoint().unwrap();
            store.compact(&mut || true).unwrap();
        }
    }
    (main, branch)
}

/// Every engine's file is sealed under the key of generation
/// `generation` (§M14.28 L4e): its header says so, and none shows a value
/// in the clear.
fn all_engine_files_under(p: &Arc<SimPlatform>, generation: u32) {
    let dir = Path::new(DIR).join("engines");
    let names = p.fs().list_dir(&dir).unwrap();
    assert!(names.len() > 3, "{names:?}");
    for name in names {
        let path = dir.join(&name);
        let f = p
            .fs()
            .open(&path, nucleus_platform::OpenMode::ReadOnly)
            .unwrap();
        let mut b = vec![0; f.size().unwrap() as usize];
        f.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(&b[..8], b"ncseal\0\x02", "{name:?}");
        assert_eq!(
            u32::from_le_bytes(b[8..12].try_into().unwrap()),
            generation,
            "{name:?}"
        );
        // A routed key (`k0100`…`k0179`), whole: three bytes alone turn
        // up in ciphertext by chance.
        let key = |w: &[u8]| w.starts_with(b"k01") && w[3..].iter().all(u8::is_ascii_digit);
        assert!(!b.windows(5).any(key), "{name:?}: a key shows");
    }
}

/// Every page of the file opens under `key` alone — its parity bit its
/// generation's — but for the free ones, which hold nothing.
fn all_pages_under(p: &Arc<SimPlatform>, key: &PageCipher) {
    let store = open(p, &opts(key.clone(), None)).unwrap();
    let pager = store.pool().pager();
    let free = pager.accounting().free;
    for id in 2..pager.page_count() {
        if !free.contains(&id) {
            pager
                .read_page(nucleus_storage::PageId(id))
                .unwrap_or_else(|e| panic!("page {id}: {e}"));
        }
    }
}

/// A new key made while the store is open: writes go on between the
/// batches sealed again; the contents never change; a reopening midway
/// needs both keys; at the end the old key is let go, and the store
/// opens under the new key alone, refusing the old.
#[test]
fn a_new_key_online() {
    let p = Arc::new(SimPlatform::new(1));
    let (mut main, branch) = store_under_the_old_key(&p);
    let store = open(&p, &opts(old_key(), None)).unwrap();
    assert_eq!(store.rekey_state(), Rekey::Idle);
    store.rekey(new_key()).unwrap();
    assert!(matches!(store.rekey_state(), Rekey::Running { .. }));
    assert!(
        store.rekey(PageCipher::new([5; 32])).is_err(),
        "a second new key while the first runs"
    );
    let mut i = 1000;
    let mut steps = 0;
    let mut go_on = || {
        // A write between batches — some of it into pages still to be
        // looked at.
        let b = batch(i);
        store.wait_durable(store.write(&b).unwrap()).unwrap();
        apply(&mut main, &b);
        i += 1;
        steps += 1;
        // Stopped once midway.
        steps != 2
    };
    let st = store.reseal(8, &mut go_on).unwrap();
    assert!(matches!(st, Rekey::Running { .. }), "{st:?}");
    assert_eq!(contents(&store), main);
    drop(store);

    // Midway: the new key alone is not enough; both are.
    let e = open(&p, &opts(new_key(), None)).err().unwrap();
    assert!(e.to_string().contains("previous key is needed"), "{e}");
    let e = open(&p, &opts(old_key(), None)).err().unwrap();
    assert!(e.to_string().contains("another key"), "{e}");
    let store = open(&p, &opts(new_key(), Some(old_key()))).unwrap();
    assert_eq!(contents(&store), main);
    assert_eq!(contents(&store.branch("b").unwrap()), branch);
    let st = store.reseal(8, &mut || true).unwrap();
    assert_eq!(st, Rekey::Idle);
    assert_eq!(contents(&store), main);
    store.pool().pager().check().unwrap();
    assert!(!p.fs().exists(&Path::new(DIR).join("data.reseal")).unwrap());
    drop(store);

    all_pages_under(&p, &new_key());
    all_engine_files_under(&p, 1);
    let e = open(&p, &opts(old_key(), None)).err().unwrap();
    assert!(e.to_string().contains("another key"), "{e}");
    let store = open(&p, &opts(new_key(), Some(old_key()))).unwrap();
    assert_eq!(
        store.rekey_state(),
        Rekey::Idle,
        "the old key not taken again"
    );
    assert_eq!(contents(&store), main);
    assert_eq!(contents(&store.branch("b").unwrap()), branch);
}

/// A store closed under the old key and opened with the new one and the
/// old as the previous: given the new key at once, its log replayed
/// under the old.
#[test]
fn a_new_key_given_as_the_store_opens() {
    let p = Arc::new(SimPlatform::new(2));
    let (main, branch) = store_under_the_old_key(&p);
    all_engine_files_under(&p, 0);
    let store = open(&p, &opts(new_key(), Some(old_key()))).unwrap();
    assert!(matches!(store.rekey_state(), Rekey::Running { .. }));
    assert_eq!(contents(&store), main);
    assert_eq!(store.reseal(8, &mut || true).unwrap(), Rekey::Idle);
    drop(store);
    all_pages_under(&p, &new_key());
    let store = open(&p, &opts(new_key(), None)).unwrap();
    assert_eq!(contents(&store), main);
    assert_eq!(contents(&store.branch("b").unwrap()), branch);
}

/// The workload of the crash test: the store under the old key given
/// the new one as it opens, writes between the batches sealed again.
/// The contents acknowledged (every write waited durable) before the
/// first error.
fn rekey_workload(p: &Arc<SimPlatform>, start: &Model) -> (Model, Option<Batch>) {
    let mut acked = start.clone();
    let Ok(store) = open(p, &opts(new_key(), Some(old_key()))) else {
        return (acked, None);
    };
    let mut i = 2000;
    let mut in_flight = None;
    let mut failed = false;
    let mut go_on = || {
        let b = batch(i);
        i += 1;
        in_flight = Some(b.clone());
        match store.write(&b).and_then(|l| store.wait_durable(l)) {
            Ok(()) => {
                apply(&mut acked, &b);
                in_flight = None;
                true
            }
            Err(_) => {
                failed = true;
                false
            }
        }
    };
    let r = store.reseal(8, &mut go_on);
    if let Ok(st) = r
        && !failed
    {
        assert_eq!(st, Rekey::Idle);
    }
    (acked, in_flight)
}

/// A crash at every I/O point of sealing a store again under a new key,
/// dropping what was not synced or tearing it: reopened with both keys
/// it holds what was acknowledged (and the write in flight, or not), and
/// the new key then finishes — after which the new key alone opens it.
#[test]
fn a_crash_at_every_point_of_a_new_key() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        let dry = Arc::new(SimPlatform::new(3));
        let (main, _) = store_under_the_old_key(&dry);
        let before = dry.sim_fs().op_count();
        rekey_workload(&dry, &main);
        let total = dry.sim_fs().op_count() - before;
        assert!(total > 50, "{total} operations");
        for n in 1..=total {
            let ctx = format!("{mode:?}: crash at op {n} of {total}");
            let p = Arc::new(SimPlatform::new(3));
            store_under_the_old_key(&p);
            p.sim_fs().set_faults(Faults {
                crash_mode: mode,
                ..Faults::default()
            });
            p.sim_fs().crash_after_ops(n);
            let (acked, in_flight) = rekey_workload(&p, &main);
            assert_eq!(p.sim_fs().crash_count(), 1, "{ctx}: did not crash");
            let store = open(&p, &opts(new_key(), Some(old_key())))
                .unwrap_or_else(|e| panic!("{ctx}: reopened: {e}"));
            let got = contents(&store);
            let mut with = acked.clone();
            if let Some(b) = &in_flight {
                apply(&mut with, b);
            }
            assert!(got == acked || got == with, "{ctx}: contents");
            assert_eq!(
                store
                    .reseal(8, &mut || true)
                    .unwrap_or_else(|e| panic!("{ctx}: {e}")),
                Rekey::Idle,
                "{ctx}"
            );
            store.pool().pager().check().unwrap();
            drop(store);
            let store = open(&p, &opts(new_key(), None))
                .unwrap_or_else(|e| panic!("{ctx}: under the new key alone: {e}"));
            assert!(contents(&store) == got, "{ctx}: after");
            drop(store);
            all_engine_files_under(&p, 1);
        }
    }
}
