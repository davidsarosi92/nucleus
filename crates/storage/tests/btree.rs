//! B+tree tests: against a model, invariants after every epoch, no page
//! leaks, and the copy-on-write tree surviving crashes.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::node::MAX_KEY;
use nucleus_storage::{BTree, BufferPool, Error, PageId, Pager};

const DB: &str = "/db/data";
const ROOT_SLOT: usize = 0;

fn platform(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(seed);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    p.fs().sync_dir(Path::new("/")).unwrap();
    p
}

fn new_tree(p: &SimPlatform, frames: usize) -> BTree {
    let pager = Arc::new(Pager::create(p.fs(), Path::new(DB)).unwrap());
    BTree::create(Arc::new(BufferPool::new(pager, frames))).unwrap()
}

/// Commits the tree: root into the meta slot, then pool flush + pager sync.
fn commit(tree: &BTree) -> Result<(), Error> {
    tree.pool().pager().set_meta(ROOT_SLOT, tree.root().0)?;
    tree.pool().sync()
}

fn reopen(p: &SimPlatform, frames: usize) -> Result<BTree, Error> {
    let pager = Arc::new(Pager::open(p.fs(), Path::new(DB))?);
    let root = pager.meta(ROOT_SLOT);
    let pool = Arc::new(BufferPool::new(pager, frames));
    if root == 0 {
        BTree::create(pool)
    } else {
        Ok(BTree::open(pool, PageId(root)))
    }
}

fn key(rng: &SplitMix64) -> Vec<u8> {
    let len = match rng.below(20) {
        0 => rng.below(MAX_KEY as u64 + 1) as usize,
        1 => 0,
        _ => 1 + rng.below(12) as usize,
    };
    // A small alphabet, so keys collide and prefixes are shared.
    (0..len)
        .map(|_| b"abcd\x00\xff"[rng.below(6) as usize])
        .collect()
}

fn value(rng: &SplitMix64) -> Vec<u8> {
    let len = match rng.below(30) {
        0 => 2000 + rng.below(30_000) as usize, // overflow pages
        1 => 0,
        2 => 1024 + rng.below(900) as usize, // compressed in the cell
        _ => rng.below(200) as usize,
    };
    let b = rng.next_u64() as u8;
    if rng.below(3) == 0 {
        // Repeats: compressed, in the cell or in overflow pages.
        let word = [b, b ^ 1, b"x"[0]];
        return (0..len).map(|i| word[i % 3]).collect();
    }
    (0..len).map(|i| b.wrapping_add(i as u8)).collect()
}

fn bound(rng: &SplitMix64) -> Bound<Vec<u8>> {
    match rng.below(3) {
        0 => Bound::Unbounded,
        1 => Bound::Included(key(rng)),
        _ => Bound::Excluded(key(rng)),
    }
}

fn as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    b.as_ref().map(Vec::as_slice)
}

/// Checks the tree's invariants, its contents against `model`, and that
/// every page of the file is accounted for exactly once.
fn verify(tree: &BTree, model: &BTreeMap<Vec<u8>, Vec<u8>>, rng: &SplitMix64) {
    let check = tree.check().unwrap();
    assert_eq!(check.entries, model.len() as u64);

    let all: Vec<_> = tree
        .range(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert!(all == want, "full scan differs from the model");
    let mut rev: Vec<_> = tree
        .range_rev(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rev.reverse();
    assert!(rev == want, "reverse scan differs from the model");

    for _ in 0..20 {
        let (lo, hi) = (bound(rng), bound(rng));
        let valid = match (&lo, &hi) {
            (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                a <= b
            }
            _ => true,
        };
        if !valid || matches!((&lo, &hi), (Bound::Excluded(a), Bound::Excluded(b)) if a == b) {
            continue;
        }
        let got: Vec<_> = tree
            .range(as_ref(&lo), as_ref(&hi))
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        let exp: Vec<_> = model
            .range((lo.clone(), hi.clone()))
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(got, exp, "range {lo:?}..{hi:?}");
        let mut got_rev: Vec<_> = tree
            .range_rev(as_ref(&lo), as_ref(&hi))
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        got_rev.reverse();
        assert_eq!(got_rev, exp, "reverse range {lo:?}..{hi:?}");
    }

    let acc = tree.pool().pager().accounting();
    let mut seen: BTreeSet<u64> = check.pages.clone();
    for id in acc
        .free
        .iter()
        .chain(&acc.pending)
        .chain(&acc.freelist_pages)
    {
        assert!(seen.insert(*id), "page {id} is both in the tree and free");
    }
    assert_eq!(seen.len() as u64, acc.page_count - 2, "pages leaked");
}

fn workload(
    tree: &mut BTree,
    model: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    rng: &SplitMix64,
    ops: usize,
) {
    for _ in 0..ops {
        let k = key(rng);
        if rng.below(3) == 0 {
            let had = model.remove(&k).is_some();
            assert_eq!(tree.remove(&k).unwrap(), had);
        } else {
            let v = value(rng);
            tree.insert(&k, &v).unwrap();
            model.insert(k, v);
        }
    }
}

#[test]
fn matches_a_model() {
    harness::run("btree_matches_a_model", 6, |seed| {
        let p = platform(seed);
        let mut tree = new_tree(&p, 32);
        let rng = SplitMix64::new(seed);
        let mut model = BTreeMap::new();
        for epoch in 0..8 {
            workload(&mut tree, &mut model, &rng, 400);
            for _ in 0..50 {
                let k = key(&rng);
                assert_eq!(tree.get(&k).unwrap(), model.get(&k).cloned());
            }
            verify(&tree, &model, &rng);
            if epoch % 2 == 1 {
                commit(&tree).unwrap(); // later epochs then copy-on-write
            }
        }
        // Drain it: the tree shrinks back to a single empty leaf.
        for k in model.keys().cloned().collect::<Vec<_>>() {
            assert!(tree.remove(&k).unwrap());
        }
        model.clear();
        verify(&tree, &model, &rng);
        assert_eq!(tree.check().unwrap().depth, 1);
    });
}

#[test]
fn grows_deep_with_long_keys() {
    let p = platform(1);
    let mut tree = new_tree(&p, 64);
    let mut model = BTreeMap::new();
    for i in 0..3000u32 {
        // Long keys that differ early (a shared prefix is stored once in
        // a leaf, so it would not make the tree deep).
        let mut k = i.reverse_bits().to_be_bytes().to_vec();
        k.extend_from_slice(&[b'k'; 600]);
        tree.insert(&k, &i.to_le_bytes()).unwrap();
        model.insert(k, i.to_le_bytes().to_vec());
    }
    let check = tree.check().unwrap();
    assert!(check.depth >= 4, "depth {}", check.depth);
    verify(&tree, &model, &SplitMix64::new(1));
}

#[test]
fn refuses_oversized_keys() {
    let p = platform(1);
    let mut tree = new_tree(&p, 8);
    assert!(matches!(
        tree.insert(&vec![0; MAX_KEY + 1], b""),
        Err(Error::KeyTooLarge(_))
    ));
    tree.insert(&vec![0; MAX_KEY], b"ok").unwrap();
}

#[test]
fn large_values_round_trip() {
    let p = platform(1);
    let mut tree = new_tree(&p, 8);
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    tree.insert(b"big", &big).unwrap();
    assert_eq!(tree.get(b"big").unwrap().unwrap(), big);
    commit(&tree).unwrap();
    tree.insert(b"big", b"small").unwrap(); // frees the chain
    commit(&tree).unwrap();
    let mut model = BTreeMap::new();
    model.insert(b"big".to_vec(), b"small".to_vec());
    verify(&tree, &model, &SplitMix64::new(1));
}

#[test]
fn the_committed_tree_survives_a_crash() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("btree_committed_tree_survives_a_crash", 8, |seed| {
            let p = platform(seed);
            p.sim_fs().set_faults(Faults {
                crash_mode: mode,
                ..Faults::default()
            });
            let rng = SplitMix64::new(seed);
            let mut tree = new_tree(&p, 16);
            let mut model = BTreeMap::new();
            workload(&mut tree, &mut model, &rng, 300);
            commit(&tree).unwrap();
            let committed = model.clone();

            // Keep changing the tree (with a small pool, so pages are written
            // back), then crash somewhere in the middle.
            p.sim_fs().crash_after_ops(1 + rng.below(400));
            let _ = (|| -> Result<(), Error> {
                for _ in 0..300 {
                    let k = key(&rng);
                    if rng.below(3) == 0 {
                        tree.remove(&k)?;
                    } else {
                        tree.insert(&k, &value(&rng))?;
                    }
                }
                Ok(())
            })();
            p.sim_fs().crash();

            let tree = reopen(&p, 16).unwrap();
            verify(&tree, &committed, &rng);
        });
    }
}

#[test]
fn crash_at_every_io_point_of_a_commit() {
    harness::run("btree_crash_at_every_io_point_of_a_commit", 2, |seed| {
        // Returns the models of every commit attempted, and how many completed.
        type Model = BTreeMap<Vec<u8>, Vec<u8>>;
        let run = |crash_at: Option<u64>| -> (SimPlatform, Vec<Model>, usize) {
            let p = platform(seed);
            p.sim_fs().set_faults(Faults {
                crash_mode: CrashMode::Torn,
                ..Faults::default()
            });
            if let Some(n) = crash_at {
                p.sim_fs().crash_after_ops(n);
            }
            let rng = SplitMix64::new(seed);
            let mut attempted = vec![BTreeMap::new()]; // the empty tree
            let mut done = 1;
            let _ = (|| -> Result<(), Error> {
                let pager = Arc::new(Pager::create(p.fs(), Path::new(DB))?);
                let mut tree = BTree::create(Arc::new(BufferPool::new(pager, 8)))?;
                let mut model = BTreeMap::new();
                for _ in 0..4 {
                    for _ in 0..60 {
                        let (k, v) = (key(&rng), value(&rng));
                        tree.insert(&k, &v)?;
                        model.insert(k, v);
                    }
                    attempted.push(model.clone());
                    commit(&tree)?;
                    done += 1;
                }
                Ok(())
            })();
            (p, attempted, done)
        };
        let setup_ops = platform(seed).sim_fs().op_count();
        let (dry, _, _) = run(None);
        let total = dry.sim_fs().op_count() - setup_ops;
        for n in 1..=total {
            let (p, attempted, done) = run(Some(n));
            assert_eq!(
                p.sim_fs().crash_count(),
                1,
                "op {n} of {total} did not crash"
            );
            let tree = match reopen(&p, 8) {
                Ok(tree) => tree,
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && done == 1 => {
                    continue;
                }
                Err(e) => panic!("crash at op {n}: reopen failed: {e}"),
            };
            // Exactly the last completed commit, or the one in flight.
            let got: Model = tree
                .range(Bound::Unbounded, Bound::Unbounded)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let in_flight = attempted.get(done);
            assert!(
                got == attempted[done - 1] || Some(&got) == in_flight,
                "crash at op {n}: tree matches neither commit {} nor {}",
                done - 1,
                done
            );
            tree.check().unwrap();
        }
    });
}

/// PLAN.md §M3 "done when": agrees with the model over a million random
/// operations, with the invariants checked along the way.  Long (~30 s), so
/// ignored by default; CI runs the ignored tests on one target.
#[test]
#[ignore = "long; run with --ignored"]
fn a_million_operations() {
    let p = platform(7);
    let mut tree = new_tree(&p, 256);
    let rng = SplitMix64::new(7);
    let mut model = BTreeMap::new();
    for round in 0..10 {
        for _ in 0..100_000 {
            let k = format!("{:06}", rng.below(50_000)).into_bytes();
            match rng.below(4) {
                0 => assert_eq!(tree.remove(&k).unwrap(), model.remove(&k).is_some()),
                1 => assert_eq!(tree.get(&k).unwrap(), model.get(&k).cloned()),
                _ => {
                    let v = vec![rng.next_u64() as u8; rng.below(64) as usize];
                    tree.insert(&k, &v).unwrap();
                    model.insert(k, v);
                }
            }
        }
        verify(&tree, &model, &rng);
        if round % 3 == 2 {
            commit(&tree).unwrap();
        }
    }
}

/// Sorted batches (`apply_sorted`, how the store applies a batch): the
/// same tree contents as one at a time, every invariant kept — new keys at
/// the end, keys between existing ones, replacements, removals (of keys
/// there and not, and of most of the tree), large values in overflow
/// pages, trees of several levels.
#[test]
fn sorted_batches_agree_with_the_model() {
    harness::run("btree_sorted_batches", 6, |seed| {
        let p = platform(seed);
        let mut tree = new_tree(&p, 64);
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let rng = SplitMix64::new(seed);
        for round in 0..30u64 {
            let mut batch = BTreeMap::new();
            let n = 1 + rng.below(if round % 5 == 0 { 3000 } else { 300 });
            for i in 0..n {
                let k = match rng.below(3) {
                    // Appends past every key so far.
                    0 => format!("z{round:04}{i:06}").into_bytes(),
                    _ => key(&rng),
                };
                batch.insert(k, value(&rng));
            }
            // Removals too: keys there, keys not, and (some rounds) most of
            // the tree, so nodes merge and the root shrinks.
            let mut ops: BTreeMap<Vec<u8>, Option<Vec<u8>>> =
                batch.into_iter().map(|(k, v)| (k, Some(v))).collect();
            let wipe = round % 7 == 6;
            for k in model.keys() {
                if wipe || rng.below(4) == 0 {
                    ops.insert(k.clone(), None);
                }
            }
            for _ in 0..rng.below(50) {
                ops.entry(key(&rng)).or_insert(None);
            }
            let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = ops.into_iter().collect();
            tree.apply_sorted(&ops).unwrap();
            for (k, v) in ops {
                match v {
                    Some(v) => {
                        model.insert(k, v);
                    }
                    None => {
                        model.remove(&k);
                    }
                }
            }
            let check = tree.check().unwrap();
            assert_eq!(check.entries, model.len() as u64, "round {round}");
            if round % 3 == 0 {
                verify(&tree, &model, &rng);
            }
        }
        verify(&tree, &model, &rng);
    });
}
