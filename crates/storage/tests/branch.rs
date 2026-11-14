//! Branch tests (PLAN.md §M14.13 B1): a branch is a writable copy that
//! shares every page it has not changed; writes on either side stay
//! invisible to the other; dropping a branch gives back what it changed
//! and what its parent kept for it; every page is in one place at every
//! step, and after a crash at any I/O point.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::{Batch, Error, MAIN_NAME, Store, StoreOptions};

const DIR: &str = "/db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
/// Every branch's contents, by name.
type Forest = BTreeMap<String, Model>;

fn opts() -> StoreOptions {
    StoreOptions {
        cache_pages: 16,
        wal_segment_size: 4096,
    }
}

fn sim(seed: u64) -> Arc<SimPlatform> {
    Arc::new(SimPlatform::new(seed))
}

fn open(p: &Arc<SimPlatform>) -> Result<Store, Error> {
    Ok(Store::open(Arc::clone(p) as Arc<dyn Platform>, Path::new(DIR), &opts())?.0)
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

fn forest(store: &Store) -> Forest {
    store
        .branches()
        .into_iter()
        .map(|b| (b.name.clone(), contents(&store.branch(&b.name).unwrap())))
        .collect()
}

fn put(store: &Store, model: &mut Model, k: &str, v: &[u8]) {
    store
        .commit(&Batch {
            meta: 0,
            ops: vec![(k.as_bytes().to_vec(), Some(v.to_vec()))],
        })
        .unwrap();
    model.insert(k.as_bytes().to_vec(), v.to_vec());
}

/// `n` keys of a few hundred bytes each: several leaves.
fn fill(store: &Store, model: &mut Model, prefix: &str, n: usize, byte: u8) {
    let ops: Vec<_> = (0..n)
        .map(|i| {
            (
                format!("{prefix}{i:04}").into_bytes(),
                Some(vec![byte; 200 + i % 300]),
            )
        })
        .collect();
    for (k, v) in &ops {
        model.insert(k.clone(), v.clone().unwrap());
    }
    store.commit(&Batch { meta: 0, ops }).unwrap();
}

fn used_pages(store: &Store) -> u64 {
    let pager = store.pool().pager();
    let a = pager.accounting();
    a.page_count - a.free.len() as u64 - a.pending.len() as u64 - a.freelist_pages.len() as u64
}

#[test]
fn a_branch_is_a_copy_and_writes_stay_on_their_side() {
    let p = sim(1);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 400, 1);
    main.checkpoint().unwrap();
    let before = used_pages(&main);

    let a = main.create_branch("a").unwrap();
    // Constant space: the branch directory's page or two, no copy.
    assert!(
        used_pages(&main) <= before + 3,
        "{} → {}",
        before,
        used_pages(&main)
    );
    let mut am = m.clone();
    assert_eq!(contents(&a), am);
    assert_eq!(a.recovery().meta, main.recovery().meta);

    put(&main, &mut m, "k0001", b"main");
    put(&a, &mut am, "k0002", b"branch");
    put(&a, &mut am, "new", b"only in a");
    assert_eq!(contents(&main), m);
    assert_eq!(contents(&a), am);
    main.check_pages().unwrap();
    main.checkpoint().unwrap();
    main.check_pages().unwrap();

    // Both survive a crash, the branch's unsynced write replayed from
    // its own log.
    put(&a, &mut am, "k0003", b"logged");
    drop((main, a));
    p.sim_fs().crash();
    let main = open(&p).unwrap();
    let names: Vec<_> = main.branches().into_iter().map(|b| b.name).collect();
    assert_eq!(names, [MAIN_NAME, "a"]);
    assert_eq!(contents(&main), m);
    assert_eq!(contents(&main.branch("a").unwrap()), am);
    main.check_pages().unwrap();
}

#[test]
fn names_are_checked() {
    let p = sim(2);
    let main = open(&p).unwrap();
    for bad in ["", MAIN_NAME, "a b", "ü", &"x".repeat(64)] {
        assert!(
            matches!(main.create_branch(bad), Err(Error::Branch(_))),
            "{bad:?}"
        );
    }
    main.create_branch("a").unwrap();
    assert!(matches!(main.create_branch("a"), Err(Error::Branch(_))));
    assert!(matches!(main.branch("nope"), Err(Error::Branch(_))));
    assert!(matches!(main.drop_branch(MAIN_NAME), Err(Error::Branch(_))));
    main.check_pages().unwrap();
}

#[test]
fn dropping_gives_back_what_the_branch_changed() {
    let p = sim(3);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 600, 1);
    main.checkpoint().unwrap();
    let a = main.create_branch("a").unwrap();
    main.checkpoint().unwrap();
    let base = used_pages(&main);

    let mut am = m.clone();
    fill(&a, &mut am, "k", 600, 2); // every leaf copied
    fill(&a, &mut am, "z", 300, 3);
    a.checkpoint().unwrap();
    assert!(used_pages(&main) > base + 40);
    main.check_pages().unwrap();

    main.drop_branch("a").unwrap();
    assert!(matches!(a.get(b"k0001"), Err(Error::Branch(_))));
    assert!(matches!(a.commit(&Batch::default()), Err(Error::Branch(_))));
    main.checkpoint().unwrap(); // frees the pages freed in the drop's commit
    main.check_pages().unwrap();
    assert!(
        used_pages(&main) <= base,
        "{} pages in use, {base} before the branch",
        used_pages(&main)
    );
    assert_eq!(contents(&main), m);
    assert_eq!(main.branches().len(), 1);
    // Its log is gone.
    let wal = Path::new("/db/branches/1/wal");
    assert!(p.fs().list_dir(wal).unwrap().is_empty());
}

#[test]
fn the_parent_keeps_what_its_branches_read() {
    let p = sim(4);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 500, 1);
    let snap1 = m.clone();
    let a = main.create_branch("a").unwrap();
    fill(&main, &mut m, "k", 250, 2); // half the leaves: on a's dead list
    let snap2 = m.clone();
    let b = main.create_branch("b").unwrap();
    fill(&main, &mut m, "k", 500, 3); // the rest: a's and b's
    main.checkpoint().unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&a), snap1);
    assert_eq!(contents(&b), snap2);
    assert_eq!(contents(&main), m);

    // Each order of drops leaves the other branch whole and the file
    // accounted for.
    main.drop_branch("b").unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&a), snap1);
    drop((a, b));
    p.sim_fs().crash();
    let main = open(&p).unwrap();
    assert_eq!(contents(&main.branch("a").unwrap()), snap1);
    main.check_pages().unwrap();
    let c = main.create_branch("c").unwrap();
    fill(&main, &mut m, "k", 500, 4);
    main.drop_branch("a").unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&c), {
        let mut s = snap2.clone();
        for i in 0..500 {
            s.insert(format!("k{i:04}").into_bytes(), vec![3; 200 + i % 300]);
        }
        s
    });
    main.drop_branch("c").unwrap();
    main.checkpoint().unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&main), m);
    // Nothing kept for branches that are gone: no more than the tree's
    // own pages, the directory's and the free list's.
    let tree = main.read().unwrap().check().unwrap().pages.len() as u64;
    assert!(
        used_pages(&main) <= tree + 4,
        "{} vs {tree}",
        used_pages(&main)
    );
}

#[test]
fn branches_of_branches() {
    let p = sim(5);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 300, 1);
    let a = main.create_branch("a").unwrap();
    let mut am = m.clone();
    fill(&a, &mut am, "k", 150, 2);
    let aa = a.create_branch("aa").unwrap();
    let mut aam = am.clone();
    fill(&aa, &mut aam, "q", 100, 3);
    fill(&a, &mut am, "k", 300, 4);
    fill(&main, &mut m, "k", 300, 5);
    main.check_pages().unwrap();
    assert_eq!(contents(&main), m);
    assert_eq!(contents(&a), am);
    assert_eq!(contents(&aa), aam);
    let info = aa.info().clone();
    assert_eq!(info.parent, Some(a.info().id));

    // A branch with branches cannot go; its branch first.
    assert!(matches!(main.drop_branch("a"), Err(Error::Branch(_))));
    assert!(matches!(main.reset_branch("a"), Err(Error::Branch(_))));
    main.drop_branch("aa").unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&a), am);
    main.drop_branch("a").unwrap();
    main.checkpoint().unwrap();
    main.check_pages().unwrap();
    assert_eq!(contents(&main), m);
}

#[test]
fn a_reset_branch_is_its_parent_now() {
    let p = sim(6);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 200, 1);
    let a = main.create_branch("a").unwrap();
    let mut am = m.clone();
    fill(&a, &mut am, "x", 100, 2);
    put(&main, &mut m, "k0000", b"later");
    let a2 = main.reset_branch("a").unwrap();
    assert!(a.get(b"x0000").is_err());
    assert_eq!(contents(&a2), m);
    assert_ne!(a2.info().id, a.info().id);
    assert_eq!(contents(&main.branch("a").unwrap()), m);
    main.check_pages().unwrap();
}

#[test]
fn branches_on_the_real_file_system() {
    let dir = std::env::temp_dir().join(format!("nucleus-branch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let platform: Arc<dyn Platform> =
        Arc::new(nucleus_platform::real::RealPlatform::new().unwrap());
    let open = || {
        Store::open(Arc::clone(&platform), &dir, &StoreOptions::default())
            .unwrap()
            .0
    };
    let main = open();
    let mut m = Model::new();
    fill(&main, &mut m, "k", 100, 1);
    let a = main.create_branch("a").unwrap();
    let mut am = m.clone();
    put(&a, &mut am, "k0000", b"a");
    drop((main, a));
    let main = open();
    assert_eq!(contents(&main), m);
    assert_eq!(contents(&main.branch("a").unwrap()), am);
    main.drop_branch("a").unwrap();
    main.check_pages().unwrap();
    drop(main);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Writes on three branches between makes, drops and resets — each
/// write a commit, so every step done is durable.  The forest after
/// each step (the one in flight last, if one failed), and how many steps
/// were done.
fn workload(p: &Arc<SimPlatform>, seed: u64) -> (Vec<Forest>, usize) {
    let rng = SplitMix64::new(seed);
    let mut history = vec![Forest::from([(MAIN_NAME.to_owned(), Model::new())])];
    let mut done = 0;
    let _ = (|| -> Result<(), Error> {
        let main = open(p)?;
        let ops = [
            "w main", "w main", "create a", "w a", "w main", "create b", "w b", "w main", "drop a",
            "w b", "reset b", "w b",
        ];
        for op in ops {
            let (verb, name) = op.split_once(' ').unwrap();
            for _ in 0..if verb == "w" { 3 } else { 1 } {
                let mut next = history[done].clone();
                match verb {
                    "w" => {
                        let store = main.branch(name)?;
                        let n = 1 + rng.below(8) as usize;
                        let ops: Vec<_> = (0..n)
                            .map(|_| {
                                let k = format!("k{:03}", rng.below(120)).into_bytes();
                                let v = (rng.below(5) != 0)
                                    .then(|| vec![rng.next_u64() as u8; rng.below(600) as usize]);
                                (k, v)
                            })
                            .collect();
                        let model = next.get_mut(name).unwrap();
                        for (k, v) in &ops {
                            match v {
                                Some(v) => model.insert(k.clone(), v.clone()),
                                None => model.remove(k),
                            };
                        }
                        history.push(next);
                        store.commit(&Batch { meta: 0, ops })?;
                    }
                    "create" | "reset" => {
                        next.insert(name.to_owned(), next[MAIN_NAME].clone());
                        history.push(next);
                        if verb == "create" {
                            main.create_branch(name)?;
                        } else {
                            main.reset_branch(name)?;
                        }
                    }
                    "drop" => {
                        next.remove(name);
                        history.push(next);
                        main.drop_branch(name)?;
                    }
                    _ => unreachable!(),
                }
                done = history.len() - 1;
            }
        }
        main.checkpoint()?;
        Ok(())
    })();
    (history, done)
}

#[test]
fn crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("branch_crash_at_every_io_point", 3, |seed| {
            let dry = sim(seed);
            let (history, done) = workload(&dry, seed);
            assert_eq!(done, history.len() - 1);
            let total = dry.sim_fs().op_count();
            {
                let main = open(&dry).unwrap();
                assert_eq!(forest(&main), history[done]);
                main.check_pages().unwrap();
            }

            for n in 1..=total {
                let p = sim(seed);
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (history, done) = workload(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                let ctx = format!("{mode:?} crash at op {n} of {total}, step {done}");
                let main = open(&p).unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
                let got = forest(&main);
                // The steps done, and maybe the one in flight.
                assert!(
                    got == history[done] || history.get(done + 1) == Some(&got),
                    "{ctx}: state is neither step's"
                );
                main.check_pages()
                    .unwrap_or_else(|e| panic!("{ctx}: pages: {e}"));
            }
        });
    }
}

/// Writers on two branches, readers, and branches made, written, dropped
/// and checkpointed alongside — on threads; every page in one place at
/// the end.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn branches_made_and_dropped_while_others_write() {
    let p = sim(7);
    let main = open(&p).unwrap();
    let mut m = Model::new();
    fill(&main, &mut m, "base", 300, 1);
    let a = main.create_branch("a").unwrap();
    std::thread::scope(|s| {
        for (t, store) in [(b'm', &main), (b'a', &a)] {
            s.spawn(move || {
                for i in 0..300u32 {
                    let k = [t, (i >> 8) as u8, i as u8].to_vec();
                    store
                        .commit(&Batch {
                            meta: 0,
                            ops: vec![
                                (k.clone(), Some(vec![t; 100])),
                                (
                                    format!("base{:04}", i % 300).into_bytes(),
                                    Some(vec![t; 50]),
                                ),
                            ],
                        })
                        .unwrap();
                    assert_eq!(store.get(&k).unwrap(), Some(vec![t; 100]));
                }
            });
        }
        let main = &main;
        s.spawn(move || {
            for i in 0..20 {
                let name = format!("t{i}");
                let t = main.create_branch(&name).unwrap();
                t.commit(&Batch {
                    meta: 0,
                    ops: vec![(b"t".to_vec(), Some(vec![i; 3000]))],
                })
                .unwrap();
                assert_eq!(t.get(b"t").unwrap(), Some(vec![i; 3000]));
                if i % 3 == 0 {
                    main.checkpoint().unwrap();
                }
                main.drop_branch(&name).unwrap();
            }
        });
    });
    let (mm, am) = (contents(&main), contents(&a));
    assert_eq!(mm.keys().filter(|k| k[0] == b'm').count(), 300);
    assert_eq!(am.keys().filter(|k| k[0] == b'a').count(), 300);
    assert!(mm.keys().all(|k| k[0] != b'a') && am.keys().all(|k| k[0] != b'm'));
    assert!(mm.values().filter(|v| v[..] == [b'm'; 50]).count() == 300);
    assert!(am.values().filter(|v| v[..] == [b'a'; 50]).count() == 300);
    main.check_pages().unwrap();
    drop((main, a));
    let main = open(&p).unwrap();
    assert_eq!(contents(&main), mm);
    assert_eq!(contents(&main.branch("a").unwrap()), am);
    assert_eq!(main.branches().len(), 2);
    main.check_pages().unwrap();
}
