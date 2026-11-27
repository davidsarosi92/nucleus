//! Savepoints (SQL's `SAVEPOINT`, issue #24): a rollback to one takes back
//! every write after it — held in memory or spilled into the store as an
//! intent — and the transaction goes on; a release keeps them.  Checked
//! against a model through random writes, nested savepoints and spills.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, Transaction};

fn opts(txn_memory: usize) -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
            ..Default::default()
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        txn_memory,
        wal_minimal: true,
    }
}

fn engine(txn_memory: usize) -> Engine {
    let p = Arc::new(SimPlatform::new(1));
    Engine::open(p as Arc<dyn Platform>, Path::new("/db"), opts(txn_memory)).unwrap()
}

fn key(i: u64) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn view(t: &Transaction) -> Model {
    t.scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn all(e: &Engine) -> Model {
    view(&e.begin())
}

/// Random writes, savepoints, releases and rollbacks to them, checked
/// against a model after each step and at the commit.
fn random_run(seed: u64, txn_memory: usize) {
    let e = engine(txn_memory);
    // Some committed rows to overwrite and delete.
    let mut base = Model::new();
    {
        let mut t = e.begin();
        for i in (0..400).step_by(3) {
            let v = format!("base{i}").into_bytes();
            t.put(&key(i), &v).unwrap();
            base.insert(key(i), v);
        }
        t.commit().unwrap();
    }
    let rng = SplitMix64::new(seed);
    let mut t = e.begin();
    let mut now = base.clone();
    let mut saved: Vec<Model> = Vec::new();
    for step in 0..3000 {
        let k = key(rng.next_u64() % 400);
        match rng.next_u64() % 20 {
            0..=7 => {
                let v = format!("s{seed}-{step}-{}", "x".repeat((step % 7) * 20)).into_bytes();
                t.put(&k, &v).unwrap();
                now.insert(k, v);
            }
            8..=10 => {
                t.delete(&k).unwrap();
                now.remove(&k);
            }
            11..=13 => {
                let v = format!("new{step}").into_bytes();
                let put = t.put_new(&k, &v).unwrap();
                assert_eq!(put, !now.contains_key(&k), "put_new at step {step}");
                if put {
                    now.insert(k, v);
                }
            }
            14 | 15 if saved.len() < 6 => {
                t.savepoint().unwrap();
                saved.push(now.clone());
            }
            16 if !saved.is_empty() => {
                t.release_savepoint();
                saved.pop();
            }
            17 | 18 if !saved.is_empty() => {
                t.rollback_to_savepoint().unwrap();
                now = saved.last().unwrap().clone();
            }
            _ => {
                let got = t.get(&k).unwrap();
                assert_eq!(got.as_ref(), now.get(&k), "get at step {step}");
            }
        }
        if step % 97 == 0 {
            assert_eq!(view(&t), now, "seed {seed} step {step}");
        }
    }
    assert_eq!(view(&t), now);
    t.commit().unwrap();
    assert_eq!(all(&e), now, "committed, seed {seed}");
    // Nothing of the transaction left behind.
    e.vacuum().unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
    assert_eq!(all(&e), now);
}

#[test]
fn a_rollback_to_a_savepoint_takes_back_what_followed_it() {
    for seed in 0..12 {
        random_run(seed, 64 << 20);
    }
}

#[test]
fn spilled_writes_are_taken_back_too() {
    for seed in 0..12 {
        random_run(100 + seed, 2048);
    }
}

#[test]
fn a_key_taken_back_is_free_for_another_writer() {
    let e = engine(2048);
    let mut a = e.begin();
    a.put(b"a", b"kept").unwrap();
    a.savepoint().unwrap();
    // Past the budget: spilled as intents.
    for i in 0..200 {
        a.put(&key(i), b"taken back").unwrap();
    }
    assert!(e.intent_count().unwrap() > 1, "spilled");
    a.rollback_to_savepoint().unwrap();
    // The intents after the savepoint deleted; the one before kept.
    assert_eq!(e.intent_count().unwrap(), 1);
    let mut b = e.begin();
    b.put(&key(5), b"b's").unwrap();
    b.commit().unwrap();
    a.commit().unwrap();
    let got = all(&e);
    assert_eq!(got.get(&b"a"[..]).map(Vec::as_slice), Some(&b"kept"[..]));
    assert_eq!(got.get(&key(5)).map(Vec::as_slice), Some(&b"b's"[..]));
    assert_eq!(got.len(), 2);
    e.vacuum().unwrap();
    assert_eq!(e.intent_count().unwrap(), 0);
}

#[test]
fn a_fresh_range_written_without_the_wal_keeps_what_preceded_the_savepoint() {
    let e = engine(64 << 20);
    let mut t = e.begin();
    t.fresh(b"i", b"j");
    for i in 0..50 {
        assert!(
            t.put_new(&[b"i".as_slice(), &key(i)].concat(), b"before")
                .unwrap()
        );
    }
    t.savepoint().unwrap();
    // Written before the savepoint (without the WAL): still there.
    assert!(
        !t.put_new(&[b"i".as_slice(), &key(10)].concat(), b"again")
            .unwrap()
    );
    for i in 50..80 {
        assert!(
            t.put_new(&[b"i".as_slice(), &key(i)].concat(), b"after")
                .unwrap()
        );
    }
    t.put(&[b"i".as_slice(), &key(3)].concat(), b"changed")
        .unwrap();
    t.rollback_to_savepoint().unwrap();
    for i in 50..60 {
        assert!(
            t.put_new(&[b"i".as_slice(), &key(i)].concat(), b"again")
                .unwrap()
        );
    }
    t.commit().unwrap();
    let got = all(&e);
    assert_eq!(got.len(), 60);
    assert_eq!(got[&[b"i".as_slice(), &key(3)].concat()], b"before");
    assert_eq!(got[&[b"i".as_slice(), &key(55)].concat()], b"again");
}

#[test]
fn without_a_savepoint_a_rollback_to_one_is_nothing() -> Result<(), Error> {
    let e = engine(64 << 20);
    let mut t = e.begin();
    t.put(b"a", b"1")?;
    t.rollback_to_savepoint()?;
    t.release_savepoint();
    t.commit()?;
    assert_eq!(all(&e).len(), 1);
    Ok(())
}
