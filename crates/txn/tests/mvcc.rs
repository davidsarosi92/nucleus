//! MVCC tests: snapshot isolation's guarantees (and its documented write
//! skew), garbage collection, recovery, concurrency, crashes.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, Timestamp, TxnOptions};

fn opts() -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 32,
            wal_segment_size: 4096,
        },
        checkpoint_wal_bytes: 16 << 10,
        history: 0,
    }
}

fn engine_on(p: &Arc<SimPlatform>, opts: EngineOptions) -> Result<Engine, Error> {
    Engine::open(Arc::clone(p) as Arc<dyn Platform>, Path::new("/db"), opts)
}

fn engine() -> (Arc<SimPlatform>, Engine) {
    let p = Arc::new(SimPlatform::new(1));
    let e = engine_on(&p, opts()).unwrap();
    (p, e)
}

fn put(e: &Engine, k: &str, v: &str) -> Timestamp {
    let mut t = e.begin();
    t.put(k.as_bytes(), v.as_bytes()).unwrap();
    t.commit().unwrap()
}

fn get(t: &nucleus_txn::Transaction, k: &str) -> Option<String> {
    t.get(k.as_bytes())
        .unwrap()
        .map(|v| String::from_utf8(v).unwrap())
}

fn scan_all(t: &nucleus_txn::Transaction) -> Vec<(String, String)> {
    t.scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            (String::from_utf8(k).unwrap(), String::from_utf8(v).unwrap())
        })
        .collect()
}

#[test]
fn reads_its_own_writes_and_rolls_back() {
    let (_p, e) = engine();
    put(&e, "a", "1");
    let mut t = e.begin();
    t.put(b"b", b"2").unwrap();
    t.delete(b"a").unwrap();
    assert_eq!(get(&t, "a"), None);
    assert_eq!(get(&t, "b").as_deref(), Some("2"));
    assert_eq!(scan_all(&t), vec![("b".into(), "2".into())]);
    t.rollback();
    let t = e.begin();
    assert_eq!(scan_all(&t), vec![("a".into(), "1".into())]);
}

#[test]
fn a_snapshot_does_not_move() {
    let (_p, e) = engine();
    put(&e, "a", "1");
    let reader = e.begin();
    put(&e, "a", "2");
    put(&e, "b", "new");
    let mut t = e.begin();
    t.delete(b"a").unwrap();
    t.commit().unwrap();
    // No dirty reads, no non-repeatable reads, no phantoms.
    assert_eq!(get(&reader, "a").as_deref(), Some("1"));
    assert_eq!(get(&reader, "b"), None);
    assert_eq!(scan_all(&reader), vec![("a".into(), "1".into())]);
    assert_eq!(scan_all(&e.begin()), vec![("b".into(), "new".into())]);
}

#[test]
fn uncommitted_writes_are_invisible() {
    let (_p, e) = engine();
    let mut writer = e.begin();
    writer.put(b"a", b"dirty").unwrap();
    let reader = e.begin();
    assert_eq!(get(&reader, "a"), None);
    writer.commit().unwrap();
    assert_eq!(get(&reader, "a"), None); // its snapshot predates the commit
    assert_eq!(get(&e.begin(), "a").as_deref(), Some("dirty"));
}

#[test]
fn first_committer_wins() {
    let (_p, e) = engine();
    put(&e, "x", "0");
    let mut t1 = e.begin();
    let mut t2 = e.begin();
    t1.put(b"x", b"t1").unwrap();
    t2.put(b"x", b"t2").unwrap();
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(err, Error::Conflict { ref key } if key == b"x") && err.is_retryable());
    assert_eq!(get(&e.begin(), "x").as_deref(), Some("t1"));
}

#[test]
fn write_skew_is_possible_under_snapshot_isolation() {
    // Two doctors on call; each goes off call if the other is still on.
    // Under snapshot isolation both succeed (documented; §M14 fixes it).
    let (_p, e) = engine();
    put(&e, "alice", "on");
    put(&e, "bob", "on");
    let mut t1 = e.begin();
    let mut t2 = e.begin();
    if get(&t1, "bob").as_deref() == Some("on") {
        t1.put(b"alice", b"off").unwrap();
    }
    if get(&t2, "alice").as_deref() == Some("on") {
        t2.put(b"bob", b"off").unwrap();
    }
    t1.commit().unwrap();
    t2.commit().unwrap();
    let t = e.begin();
    assert_eq!(
        (get(&t, "alice").as_deref(), get(&t, "bob").as_deref()),
        (Some("off"), Some("off"))
    );
}

#[test]
fn lost_updates_cannot_happen() {
    let (_p, e) = engine();
    put(&e, "counter", "0");
    let threads = 6;
    let per = 25;
    std::thread::scope(|s| {
        for _ in 0..threads {
            let e = e.clone();
            s.spawn(move || {
                for _ in 0..per {
                    loop {
                        let mut t = e.begin();
                        let n: u64 = get(&t, "counter").unwrap().parse().unwrap();
                        // A conflict can surface at the write or the commit.
                        let r = t
                            .put(b"counter", (n + 1).to_string().as_bytes())
                            .and_then(|()| t.commit());
                        match r {
                            Ok(_) => break,
                            Err(err) if err.is_retryable() => continue,
                            Err(err) => panic!("{err}"),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(
        get(&e.begin(), "counter"),
        Some((threads * per).to_string())
    );
}

#[test]
fn transfers_keep_the_total() {
    let (_p, e) = engine();
    let accounts = 8u64;
    let mut t = e.begin();
    for a in 0..accounts {
        t.put(format!("acct{a}").as_bytes(), b"100").unwrap();
    }
    t.commit().unwrap();
    let total = |t: &nucleus_txn::Transaction| -> u64 {
        scan_all(t)
            .iter()
            .map(|(_, v)| v.parse::<u64>().unwrap())
            .sum()
    };
    std::thread::scope(|s| {
        for id in 0..4u64 {
            let e = e.clone();
            s.spawn(move || {
                let rng = SplitMix64::new(id);
                for _ in 0..40 {
                    loop {
                        let mut t = e.begin();
                        let (from, to) = (rng.below(accounts), rng.below(accounts));
                        let (fk, tk) = (format!("acct{from}"), format!("acct{to}"));
                        let f: u64 = get(&t, &fk).unwrap().parse().unwrap();
                        let amount = rng.below(f + 1);
                        // A conflict can surface at a write or the commit.
                        let r = t
                            .put(fk.as_bytes(), (f - amount).to_string().as_bytes())
                            .and_then(|()| {
                                let g: u64 = get(&t, &tk).unwrap().parse().unwrap();
                                t.put(tk.as_bytes(), (g + amount).to_string().as_bytes())
                            })
                            .and_then(|()| t.commit().map(|_| ()));
                        match r {
                            Ok(()) => break,
                            Err(err) if err.is_retryable() => continue,
                            Err(err) => panic!("{err}"),
                        }
                    }
                }
            });
        }
        let e = e.clone();
        s.spawn(move || {
            for _ in 0..100 {
                assert_eq!(
                    total(&e.begin()),
                    accounts * 100,
                    "a snapshot saw money appear or vanish"
                );
            }
        });
    });
    assert_eq!(total(&e.begin()), accounts * 100);
}

#[test]
fn old_versions_are_collected() {
    let (_p, e) = engine();
    for i in 0..20 {
        put(&e, "k", &i.to_string());
    }
    // Each commit collected all but the version its own snapshot read: two
    // left, and vacuum takes the older one.
    assert_eq!(e.version_count().unwrap(), 2);
    assert_eq!(e.vacuum().unwrap(), 1);
    assert_eq!(e.version_count().unwrap(), 1);

    let old = e.begin();
    for i in 20..30 {
        put(&e, "k", &i.to_string());
    }
    // The open snapshot pins the version it sees, and everything newer.
    assert_eq!(get(&old, "k").as_deref(), Some("19"));
    assert_eq!(e.version_count().unwrap(), 11);
    drop(old);
    put(&e, "k", "last");
    assert_eq!(e.version_count().unwrap(), 2);

    // Deleted keys leave a tombstone until vacuum sweeps it.
    let mut t = e.begin();
    t.delete(b"k").unwrap();
    t.commit().unwrap();
    assert_eq!(e.vacuum().unwrap(), 2); // the tombstone and the value under it
    assert_eq!(e.version_count().unwrap(), 0);
}

#[test]
fn history_keeps_old_snapshots_readable() {
    let p = Arc::new(SimPlatform::new(1));
    let e = engine_on(
        &p,
        EngineOptions {
            history: 100,
            ..opts()
        },
    )
    .unwrap();
    let t1 = put(&e, "k", "v1");
    put(&e, "k", "v2");
    e.vacuum().unwrap();
    let old = e
        .begin_with(TxnOptions {
            at: Some(t1),
            ..TxnOptions::default()
        })
        .unwrap();
    assert_eq!(get(&old, "k").as_deref(), Some("v1"));
    let mut old = old;
    assert!(matches!(old.put(b"x", b"y"), Err(Error::ReadOnly)));

    let (_p, e) = engine(); // no history
    let t1 = put(&e, "k", "v1");
    put(&e, "k", "v2");
    put(&e, "k", "v3");
    assert!(matches!(
        e.begin_with(TxnOptions {
            at: Some(t1),
            ..TxnOptions::default()
        }),
        Err(Error::SnapshotTooOld)
    ));
}

#[test]
fn keys_with_zero_bytes_and_prefixes() {
    let (_p, e) = engine();
    let keys: Vec<&[u8]> = vec![
        b"",
        b"\x00",
        b"\x00\x00",
        b"\x00\xff",
        b"a",
        b"a\x00",
        b"ab",
    ];
    let mut t = e.begin();
    for k in &keys {
        t.put(k, k).unwrap();
    }
    t.commit().unwrap();
    let t = e.begin();
    let got: Vec<Vec<u8>> = t
        .scan(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(|r| r.unwrap().0)
        .collect();
    let mut want: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
    want.sort();
    assert_eq!(got, want);
    let got: Vec<Vec<u8>> = t
        .scan(Bound::Included(b"a"), Bound::Excluded(b"ab"))
        .unwrap()
        .map(|r| r.unwrap().0)
        .collect();
    assert_eq!(got, vec![b"a".to_vec(), b"a\x00".to_vec()]);
    for k in &keys {
        assert_eq!(t.get(k).unwrap().as_deref(), Some(*k));
    }
}

#[test]
fn scans_match_a_model() {
    harness::run("txn_scans_match_a_model", 8, |seed| {
        let p = Arc::new(SimPlatform::new(seed));
        let e = engine_on(&p, opts()).unwrap();
        let rng = SplitMix64::new(seed);
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let key = |rng: &SplitMix64| format!("{:03}", rng.below(300)).into_bytes();
        for _ in 0..30 {
            let mut t = e.begin();
            let mut local = model.clone();
            for _ in 0..20 {
                let k = key(&rng);
                if rng.below(3) == 0 {
                    t.delete(&k).unwrap();
                    local.remove(&k);
                } else {
                    let v = rng.next_u64().to_le_bytes().to_vec();
                    t.put(&k, &v).unwrap();
                    local.insert(k, v);
                }
            }
            // Mid-transaction scans merge its writes with the snapshot.
            let (a, b) = (key(&rng), key(&rng));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let got: Vec<_> = t
                .scan(Bound::Included(&lo), Bound::Excluded(&hi))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let want: Vec<_> = local
                .range(lo.clone()..hi.clone())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(got, want);
            if rng.below(4) == 0 {
                t.rollback();
            } else {
                t.commit().unwrap();
                model = local;
            }
        }
        let got: Vec<_> = e
            .begin()
            .scan(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, model.into_iter().collect::<Vec<_>>());
    });
}

#[test]
fn recovery_keeps_commits_and_timestamps() {
    let (p, e) = engine();
    put(&e, "a", "1");
    let ts = put(&e, "b", "2");
    drop(e);
    p.sim_fs().crash();
    let e = engine_on(&p, opts()).unwrap();
    assert_eq!(e.visible(), ts);
    let t = e.begin();
    assert_eq!(
        scan_all(&t),
        vec![("a".into(), "1".into()), ("b".into(), "2".into())]
    );
    assert!(put(&e, "c", "3") > ts);
}

#[test]
fn crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("txn_crash_at_every_io_point", 3, |seed| {
            type Model = BTreeMap<Vec<u8>, Vec<u8>>;
            let run = |p: &Arc<SimPlatform>| -> (Vec<Model>, usize) {
                let rng = SplitMix64::new(seed);
                let mut states = vec![Model::new()];
                let mut acked = 0;
                let _ = (|| -> Result<(), Error> {
                    let e = engine_on(p, opts())?;
                    let mut model = Model::new();
                    for i in 1..=12 {
                        let mut t = e.begin();
                        for _ in 0..4 {
                            let k = format!("k{}", rng.below(10)).into_bytes();
                            if rng.below(3) == 0 {
                                t.delete(&k)?;
                                model.remove(&k);
                            } else {
                                t.put(&k, &[i as u8; 50])?;
                                model.insert(k, vec![i as u8; 50]);
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
            for n in 1..=total {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (_, acked) = run(&p);
                let e =
                    engine_on(&p, opts()).unwrap_or_else(|err| panic!("{mode:?} op {n}: {err}"));
                let k = e.visible().0 as usize;
                assert!(
                    k >= acked && k < states.len(),
                    "{mode:?} op {n}: at {k}, acked {acked}"
                );
                let got: Model = e
                    .begin()
                    .scan(Bound::Unbounded, Bound::Unbounded)
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                assert!(
                    got == states[k],
                    "{mode:?} op {n}: state is not that of commit {k}"
                );
            }
        });
    }
}
