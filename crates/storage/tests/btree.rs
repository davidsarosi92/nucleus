//! B+tree tests: against a model, invariants after every epoch, no page
//! leaks, and the copy-on-write tree surviving crashes.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::btree::{LeafEntry, LeafFilter};
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
        leaf_entries_agree(tree, model, &lo, &hi, rng);
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

/// A range read a leaf at a time (`Range::leaf_entries`, as a scan that
/// counts reads it) gives the model's entries: each key, the one before it,
/// a value of at most 3 bytes; an entry declined at random comes again.
fn leaf_entries_agree(
    tree: &BTree,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    lo: &Bound<Vec<u8>>,
    hi: &Bound<Vec<u8>>,
    rng: &SplitMix64,
) {
    let exp: Vec<_> = model.range((lo.clone(), hi.clone())).collect();
    let mut range = tree.range(as_ref(lo), as_ref(hi)).unwrap();
    let (mut key, mut prev) = (Vec::new(), Vec::new());
    let mut got: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut prevs: Vec<Vec<u8>> = Vec::new();
    loop {
        let more = range
            .leaf_entries(&mut key, &mut prev, 3, &mut |p, k, small| {
                if rng.below(7) == 0 {
                    return Ok(false);
                }
                prevs.push(p.to_vec());
                got.push((k.to_vec(), small.map(<[u8]>::to_vec)));
                Ok(true)
            })
            .unwrap()
            .expect("an ascending range gives its leaves");
        if !more {
            break;
        }
    }
    assert_eq!(got.len(), exp.len(), "leaf_entries {lo:?}..{hi:?}");
    for (i, ((k, small), (ek, ev))) in got.iter().zip(&exp).enumerate() {
        assert_eq!(k, *ek);
        assert_eq!(small, &(ev.len() <= 3).then(|| ev.to_vec()), "{k:?}");
        if i > 0 {
            assert_eq!(prevs[i], got[i - 1].0, "the key before {k:?}");
        }
    }
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

/// Sorted batches applied with leaves prepared on workers make the tree
/// one thread makes — the same root, the same pages, byte for byte, the
/// same pages free — through inserts, replacements (overflow values too),
/// removals that empty leaves, and commits between (so some pages are
/// rewritten in place, some copied).
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn batches_on_workers_make_the_same_tree() {
    same_tree_on_workers(false, 4096);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn batches_on_workers_through_a_small_pool_make_the_same_tree() {
    // Workers pinning the leaves they read while the walk takes and frees
    // pages, through a pool that evicts all the time.
    same_tree_on_workers(true, 48);
}

/// Drops every entry whose key ends in a multiple of 5 (a pure function of
/// the leaf, as a filter must be).
struct DropFifths;

impl LeafFilter for DropFifths {
    fn drops(&self, entries: &[LeafEntry], drop: &mut [bool]) {
        for (e, d) in entries.iter().zip(drop) {
            *d = e.key.last().is_some_and(|b| b % 5 == 0);
        }
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn filtered_batches_on_workers_make_the_same_tree() {
    same_tree_on_workers(true, 4096);
}

/// A tree built by batches on one worker and on four: the same pages,
/// byte for byte — `filtered`, with [`DropFifths`] dropping entries of the
/// leaves rewritten; through a pool of `frames`.
fn same_tree_on_workers(filtered: bool, frames: usize) {
    let build = |workers: usize| {
        let p = platform(41);
        let mut tree = new_tree(&p, frames);
        tree.set_workers(workers);
        let rng = SplitMix64::new(7);
        let mut model = BTreeMap::new();
        for round in 0..12 {
            let mut ops: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
            let n = 200 + rng.below(6000) as usize;
            for _ in 0..n {
                let k = if round > 0 && rng.below(2) == 0 {
                    // An existing key: replaced or removed.
                    let i = rng.below(model.len().max(1) as u64) as usize;
                    match model.keys().nth(i) {
                        Some(k) => Vec::clone(k),
                        None => continue,
                    }
                } else {
                    // Long keys: internal nodes of few children, so a
                    // deep tree.
                    let mut k = vec![b'r'; 120];
                    k.extend_from_slice(&rng.below(200_000).to_be_bytes());
                    k
                };
                let v = if rng.below(4) == 0 {
                    None
                } else {
                    Some(value(&rng))
                };
                ops.insert(k, v);
            }
            if round % 4 == 3 {
                // A removal of most keys, leaving leaves under the minimum.
                for k in model.keys().filter(|_| rng.below(10) < 8) {
                    ops.insert(k.clone(), None);
                }
            }
            let ops: Vec<_> = ops.into_iter().collect();
            if filtered {
                tree.apply_sorted_filtered(&ops, &DropFifths).unwrap();
            } else {
                tree.apply_sorted(&ops).unwrap();
            }
            for (k, v) in ops {
                match v {
                    Some(v) => model.insert(k, v),
                    None => model.remove(&k),
                };
            }
            if round % 3 == 1 {
                commit(&tree).unwrap();
            }
        }
        if filtered {
            // What is there is as written; what is not, the filter dropped
            // (and some of what it would drop is in leaves not rewritten).
            let mut have = BTreeMap::new();
            for e in tree.range(Bound::Unbounded, Bound::Unbounded).unwrap() {
                let (k, v) = e.unwrap();
                assert_eq!(model.get(&k), Some(&v), "an entry as written");
                have.insert(k, v);
            }
            let gone = model.keys().filter(|k| !have.contains_key(*k));
            assert!(gone.clone().all(|k| k.last().unwrap() % 5 == 0));
            assert!(gone.count() > 100, "the filter dropped entries");
        } else {
            verify(&tree, &model, &rng);
        }
        let check = tree.check().unwrap();
        let pages: Vec<(u64, Vec<u8>)> = check
            .pages
            .iter()
            .map(|&id| {
                let r = tree.pool().fetch(PageId(id)).unwrap();
                let page = r.read();
                (id, page.payload().to_vec())
            })
            .collect();
        let acc = tree.pool().pager().accounting();
        (tree.root(), pages, acc.free, acc.pending, check.depth)
    };
    let one = build(1);
    let before = nucleus_storage::btree::parallel_batches();
    let many = build(4);
    assert!(
        nucleus_storage::btree::parallel_batches() - before >= 6,
        "leaves prepared on workers: {} batches",
        nucleus_storage::btree::parallel_batches() - before
    );
    assert!(one.4 >= 3, "a tree deep enough for internal nodes");
    assert_eq!(one.0, many.0, "the same root");
    assert_eq!(one.1.len(), many.1.len(), "as many pages");
    assert!(one.1 == many.1, "the same pages, byte for byte");
    assert_eq!((one.2, one.3), (many.2, many.3), "the same pages free");
}

#[test]
fn a_scan_reads_ahead_and_a_short_range_does_not() {
    let p = platform(41);
    let mut tree = new_tree(&p, 4096);
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..50_000u64)
        .map(|i| (i.to_be_bytes().to_vec(), vec![i as u8; 40]))
        .collect();
    tree.insert_sorted(&entries).unwrap();
    commit(&tree).unwrap();
    // Short ranges, far apart, on a cold pool: each reads the leaves it
    // needs (at most two of ~400) and its path, no leaves ahead.
    let tree = reopen(&p, 4096).unwrap();
    for r in 0..20u64 {
        let lo = (r * 2400 + 7 * r).to_be_bytes();
        let hi = (r * 2400 + 7 * r + 50).to_be_bytes();
        let mut range = tree
            .range(Bound::Included(&lo[..]), Bound::Excluded(&hi[..]))
            .unwrap();
        let (mut k, mut v) = (Vec::new(), Vec::new());
        let mut n = 0;
        while range.next_into(&mut k, &mut v).unwrap() {
            n += 1;
        }
        assert_eq!(n, 50);
    }
    let s = tree.pool().stats();
    assert!(s.misses <= 20 * 2 + 8, "{s:?}");
    // A full scan on a cold pool: most leaves arrive read ahead, so their
    // fetches hit (besides their parents' — without it, about one hit per
    // miss).
    let tree = reopen(&p, 4096).unwrap();
    let mut range = tree.range(Bound::Unbounded, Bound::Unbounded).unwrap();
    let (mut k, mut v) = (Vec::new(), Vec::new());
    let mut n = 0u64;
    while range.next_into(&mut k, &mut v).unwrap() {
        assert_eq!(k, n.to_be_bytes());
        n += 1;
    }
    assert_eq!(n, 50_000);
    let s = tree.pool().stats();
    assert!(s.hits * 10 > s.misses * 18, "{s:?}");
}

/// `split_keys` as it was before it searched each node (§M14.20 W8): every
/// child of every node on a level looked at.
fn split_keys_by_scanning(
    tree: &BTree,
    start: &[u8],
    end: Option<&[u8]>,
    parts: usize,
    min_leaves: usize,
) -> Vec<Vec<u8>> {
    use nucleus_storage::node::View;
    let inside = |k: &[u8]| k > start && end.is_none_or(|e| k < e);
    let mut level = vec![tree.root()];
    let mut keys: Vec<Vec<u8>> = Vec::new();
    loop {
        let (mut next, mut found, mut leaves) = (Vec::new(), Vec::new(), false);
        for &id in &level {
            let r = tree.pool().fetch(id).unwrap();
            let page = r.read();
            let v = View::new(&page).unwrap();
            if v.is_leaf() {
                leaves = true;
                break;
            }
            for i in 0..=v.len() {
                // Whole keys: an internal node's have a prefix too.
                let lo = (i > 0).then(|| v.key(i - 1).unwrap().to_vec());
                let hi = (i < v.len()).then(|| v.key(i).unwrap().to_vec());
                let after_start = hi.is_none_or(|h| h.as_slice() > start);
                let before_end = match (&lo, end) {
                    (Some(l), Some(e)) => l.as_slice() < e,
                    _ => true,
                };
                if after_start && before_end {
                    next.push(v.child(i).unwrap());
                    if let Some(l) = lo
                        && inside(&l)
                    {
                        found.push(l);
                    }
                }
            }
        }
        if leaves {
            break;
        }
        keys = found;
        if next.len() > 4096 {
            break;
        }
        level = next;
    }
    let parts = parts.min((keys.len() + 1) / min_leaves.max(1)).max(1);
    (1..parts)
        .map(|j| keys[j * keys.len() / parts].clone())
        .collect()
}

#[test]
fn split_keys_agree_with_scanning_each_node() {
    harness::run("btree_split_keys", 4, |seed| {
        let p = platform(seed);
        let mut tree = new_tree(&p, 256);
        let rng = SplitMix64::new(seed);
        // Ordered keys of a few widths: trees three and four levels deep.
        let width = 8 + rng.below(40) as usize;
        let n = 20_000 + rng.below(40_000) as usize;
        for i in 0..n {
            let mut k = format!("{:0width$}", i * 7).into_bytes();
            k.extend_from_slice(&[b'x'; 3]);
            tree.insert(&k, b"value").unwrap();
        }
        // Bounds between keys, and on them (a node's separator is one).
        let at = |i: u64| {
            let mut k = format!("{:0width$}", i).into_bytes();
            if rng.below(2) == 0 {
                k = format!("{:0width$}", i / 7 * 7).into_bytes();
                k.extend_from_slice(&[b'x'; 3]);
            }
            k
        };
        for _ in 0..300 {
            let a = rng.below(n as u64 * 8);
            let b = a + rng.below(n as u64 * 8 + 1 - a);
            let start = if rng.below(8) == 0 { Vec::new() } else { at(a) };
            let end = (rng.below(8) != 0).then(|| at(b));
            for (parts, min_leaves) in [(2, 1), (8, 1), (48, 4), (1000, 1)] {
                assert_eq!(
                    tree.split_keys(&start, end.as_deref(), parts, min_leaves)
                        .unwrap(),
                    split_keys_by_scanning(&tree, &start, end.as_deref(), parts, min_leaves),
                    "{start:?}..{end:?} in {parts}"
                );
            }
        }
    });
}

/// Two leaves merged when one ran small, their keys sharing a long prefix
/// each but not with the other's: the merge, prefix-less, outgrows two
/// pages — split until each piece fits (it once split in two, and a half
/// overflowed its page).
#[test]
fn a_merge_of_prefixed_leaves_splits_until_it_fits() {
    let p = platform(7);
    let mut tree = new_tree(&p, 256);
    let key = |region: u8, i: u16| {
        let mut k = vec![region];
        k.extend(std::iter::repeat_n(region, 300));
        k.extend(i.to_be_bytes());
        k
    };
    let n = 2000u16;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for region in *b"ab" {
        entries.extend((0..n).map(|i| (key(region, i), vec![region])));
    }
    tree.insert_sorted(&entries).unwrap();
    // All of the first region but its last few: they run small and meet
    // the second region's first leaf.
    let removed: Vec<(Vec<u8>, Option<Vec<u8>>)> =
        (0..n - 3).map(|i| (key(b'a', i), None)).collect();
    tree.apply_sorted(&removed).unwrap();
    tree.check().unwrap();
    for i in n - 3..n {
        assert_eq!(tree.get(&key(b'a', i)).unwrap(), Some(vec![b'a']));
    }
    for i in 0..n {
        assert_eq!(tree.get(&key(b'b', i)).unwrap(), Some(vec![b'b']));
    }
    assert_eq!(tree.get(&key(b'a', 0)).unwrap(), None);
}

/// The same, one key at a time (`remove`, as dropping a branch's entries
/// removes them): a leaf left small is merged with its sibling, and the
/// pair, prefix-less, is cut into pieces that fit — even halves of it
/// once overflowed a page (a backup's branch drop panicked encoding one).
#[test]
fn a_removal_rebalances_prefixed_leaves_into_pieces_that_fit() {
    let p = platform(8);
    let mut tree = new_tree(&p, 256);
    let key = |region: u8, i: u16| {
        let mut k = vec![region];
        k.extend(std::iter::repeat_n(region, 300));
        k.extend(i.to_be_bytes());
        k
    };
    let n = 2000u16;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for region in *b"ab" {
        entries.extend((0..n).map(|i| (key(region, i), vec![region])));
    }
    tree.insert_sorted(&entries).unwrap();
    for i in 0..n - 3 {
        assert!(tree.remove(&key(b'a', i)).unwrap());
    }
    tree.check().unwrap();
    for i in n - 3..n {
        assert_eq!(tree.get(&key(b'a', i)).unwrap(), Some(vec![b'a']));
    }
    for i in 0..n {
        assert_eq!(tree.get(&key(b'b', i)).unwrap(), Some(vec![b'b']));
    }
}

/// `seek_group` finds what a range from each key finds first, where that
/// entry is in the key's leaf (issue #1): keys present and not, past the
/// last, values inline, compressed and in overflow pages.
#[test]
fn a_group_sought_together_finds_each_ones_first_entry() {
    use nucleus_storage::btree::Found;
    harness::run("btree_seek_group", 4, |seed| {
        let p = platform(seed);
        let mut tree = new_tree(&p, 512);
        let rng = SplitMix64::new(seed);
        let value = |i: u64| match i % 7 {
            0 => vec![b'z'; 5000],                  // overflow pages
            1 => vec![b'c'; 1500],                  // compressed
            _ => format!("value {i}").into_bytes(), // inline
        };
        for _ in 0..6000 {
            let i = rng.below(20_000);
            tree.insert(format!("key{i:06}").as_bytes(), &value(i))
                .unwrap();
        }
        let mut found = Vec::<Found>::new();
        // No keys: nothing found (it once never ended).
        tree.seek_group(&[], &mut found).unwrap();
        for _ in 0..300 {
            let n = 1 + rng.below(10) as usize;
            let keys: Vec<Vec<u8>> = (0..n)
                .map(|_| format!("key{:06}", rng.below(20_200)).into_bytes())
                .collect();
            let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
            tree.seek_group(&refs, &mut found).unwrap();
            for (j, k) in keys.iter().enumerate() {
                let first = tree
                    .range(Bound::Included(k), Bound::Unbounded)
                    .unwrap()
                    .next()
                    .transpose()
                    .unwrap();
                let f = &found[j];
                if f.at {
                    assert_eq!(Some((f.key.clone(), f.value.clone())), first, "{k:?}");
                } else {
                    // Past its leaf's last entry: the next leaf's first,
                    // or none.
                    assert!(first.as_ref().is_none_or(|(fk, _)| fk > k));
                }
            }
        }
    });
}

/// A rekeying (issue #4) on one worker and on four: every key in its range
/// that ends with an old tail ends with the new one, its value kept; the
/// keys that would pass a neighbour given back to move; the tree whole —
/// its pages written over where writable, copied where a commit holds
/// them.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_rekeying_on_workers_rewrites_key_tails() {
    let key = |u: u64, suffix: u64| {
        // Long keys: a deep tree, internal nodes of few children.
        let mut k = vec![b'r'; 100];
        k.extend_from_slice(&u.to_be_bytes());
        k.extend_from_slice(&[0, 0]);
        k.extend_from_slice(&suffix.to_be_bytes());
        k
    };
    let tail = |x: u64| {
        let mut t = vec![0, 0];
        t.extend_from_slice(&x.to_be_bytes());
        t
    };
    let tails = nucleus_storage::Tails::new(vec![(tail(5), tail(u64::MAX - 100))]);
    let (start, end) = (key(1_000, 0), key(18_000, 0));
    let run = |workers: usize| {
        let p = platform(43);
        let mut tree = new_tree(&p, 256);
        tree.set_workers(workers);
        let rng = SplitMix64::new(9);
        let mut model = BTreeMap::new();
        for half in 0..2u64 {
            let mut ops = Vec::new();
            for u in half * 10_000..(half + 1) * 10_000 {
                if u % 3 != 0 {
                    ops.push((key(u, 5), Some(value(&rng))));
                }
                if u % 11 == 0 {
                    // Newer than the commit: the intent's new key passes it.
                    ops.push((key(u, u64::MAX - 200), Some(b"newer".to_vec())));
                }
            }
            ops.sort();
            tree.apply_sorted(&ops).unwrap();
            for (k, v) in ops {
                model.insert(k, v.unwrap());
            }
            if half == 0 {
                // The first half's pages held by a commit: copied; the
                // second's written over.
                commit(&tree).unwrap();
            }
        }
        let back = tree.rekey(&start, Some(&end), &tails).unwrap();
        assert!(!back.is_empty(), "some keys pass a neighbour");
        for (old, new) in &back {
            assert!(tails.new_end(old).is_some() && new != old);
            let v = tree.get(old).unwrap().unwrap();
            tree.apply_sorted(&[(old.clone(), None), (new.clone(), Some(v))])
                .unwrap();
        }
        tree.check().unwrap();
        commit(&tree).unwrap();
        let got: BTreeMap<Vec<u8>, Vec<u8>> = tree
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (k, v) in &got {
            assert_eq!(tree.get(k).unwrap().as_ref(), Some(v));
        }
        (model, got)
    };
    let (model, one) = run(1);
    let mut want = BTreeMap::new();
    for (k, v) in model {
        let mut k = k;
        if k >= start && k < end && k.ends_with(&tail(5)) {
            let at = k.len() - 10;
            k[at..].copy_from_slice(&tail(u64::MAX - 100));
        }
        want.insert(k, v);
    }
    assert_eq!(one, want);
    let (_, four) = run(4);
    assert_eq!(four, want);
}

/// A root a thread reads from its own copy (`BufferPool::read_copy`)
/// follows the root's changes: one changed in place (allocated since the
/// last commit), one replaced by a copy, a new root grown.
#[test]
fn a_root_read_from_a_copy_follows_its_changes() {
    let p = platform(31);
    let mut tree = new_tree(&p, 4096);
    let k = |i: u32| format!("key{i:08}").into_bytes();
    let found = |tree: &BTree, i: u32| {
        tree.range(Bound::Included(&k(i)[..]), Bound::Unbounded)
            .unwrap()
            .next()
            .map(|e| e.unwrap().0)
            == Some(k(i))
    };
    for round in 0..6u32 {
        // Every other key, then a commit (the root durable: a change
        // copies it), then the keys between (in place: splits add
        // children to the root as it is).
        for i in (0..20_000).step_by(2) {
            tree.insert(&k(round * 20_000 + i), b"v").unwrap();
        }
        // Read twice: copied the second time.
        for _ in 0..3 {
            assert!(found(&tree, round * 20_000 + 10_000));
        }
        if round % 2 == 0 {
            commit(&tree).unwrap();
        }
        for i in (1..20_000).step_by(2) {
            tree.insert(&k(round * 20_000 + i), b"w").unwrap();
        }
        for i in (0..(round + 1) * 20_000).step_by(97) {
            assert!(found(&tree, i), "round {round}: key {i}");
        }
    }
}

/// A read of the first entry at or after a key without a range
/// (`BTree::seek_one`) finds what a range's first entry is — its key and
/// its value, inline or overflowing its leaf — or says its leaf has none
/// past the key; on a tree of one leaf, and on one of many levels whose
/// root is read from the thread's copy, as it changes.
#[test]
fn a_first_entry_read_alone_is_a_ranges_first() {
    let p = platform(37);
    let mut tree = new_tree(&p, 4096);
    let k = |i: u32| format!("key{i:08}").into_bytes();
    let value = |i: u32| {
        let n = if i.is_multiple_of(61) {
            9000
        } else {
            (i % 40) as usize
        };
        vec![(i % 251) as u8; n]
    };
    let check = |tree: &BTree, probe: &[u8]| -> bool {
        let mut found = nucleus_storage::btree::Found::default();
        tree.seek_one(probe, &mut found).unwrap();
        let first = tree
            .range(Bound::Included(probe), Bound::Unbounded)
            .unwrap()
            .next()
            .map(|e| e.unwrap());
        if found.at {
            assert_eq!(Some((found.key.clone(), found.value.clone())), first);
        }
        found.at
    };
    let rng = SplitMix64::new(5);
    let mut keys = 0u32;
    for round in 0..4u32 {
        let n = if round == 0 { 20 } else { 15_000 };
        for _ in 0..n {
            let i = rng.below(1_000_000) as u32 * 2;
            tree.insert(&k(i), &value(i)).unwrap();
            keys += 1;
        }
        if round == 2 {
            commit(&tree).unwrap();
        }
        let (mut at, mut tried) = (0, 0);
        for _ in 0..3000 {
            // Keys present, and keys between (odd: never inserted).
            let i = rng.below(2_000_001) as u32;
            at += usize::from(check(&tree, &k(i)));
            tried += 1;
        }
        // Past the last key: none, anywhere.
        let mut found = nucleus_storage::btree::Found::default();
        tree.seek_one(b"z", &mut found).unwrap();
        assert!(!found.at);
        // Most probes end in a leaf with an entry past them.
        assert!(
            at * 10 > tried * 9,
            "round {round}: {at} of {tried} ({keys} keys)"
        );
    }
}
