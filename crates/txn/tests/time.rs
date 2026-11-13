//! Time travel's engine (PLAN.md §M14.14 TT1): each commit's time
//! recorded while a retention by time is set, a time read as the snapshot
//! of the last commit at or before it, the retention kept and no more.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64, harness};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, Timestamp, TxnOptions};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn opts(history_time: Duration) -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 8192,
        },
        checkpoint_wal_bytes: 32 << 10,
        history: 0,
        history_time: history_time.as_micros() as u64,
        txn_memory: 64 << 20,
        wal_minimal: true,
    }
}

fn open(p: &Arc<SimPlatform>, history_time: Duration) -> Result<Engine, Error> {
    Engine::open(
        Arc::clone(p) as Arc<dyn Platform>,
        Path::new("/db"),
        opts(history_time),
    )
}

fn read_at(e: &Engine, ts: Timestamp) -> Result<Model, Error> {
    let t = e.begin_with(TxnOptions {
        at: Some(ts),
        ..TxnOptions::default()
    })?;
    t.scan(Bound::Unbounded, Bound::Unbounded)?.collect()
}

/// Random commits, the clock moving between them: each commit's time and
/// the state it left.
fn workload(
    p: &Arc<SimPlatform>,
    e: &Engine,
    rng: &SplitMix64,
    n: usize,
) -> Vec<(SystemTime, Model)> {
    let mut model = Model::new();
    let mut log = Vec::new();
    for i in 0..n {
        // Now and then several commits within one microsecond.
        if rng.below(4) != 0 {
            p.clock()
                .sleep(Duration::from_micros(1 + rng.below(5_000_000)));
        }
        let mut t = e.begin();
        for _ in 0..1 + rng.below(4) {
            let k = format!("k{}", rng.below(30)).into_bytes();
            if rng.below(4) == 0 {
                t.delete(&k).unwrap();
                model.remove(&k);
            } else {
                let v = format!("{i}").into_bytes();
                t.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        t.commit().unwrap();
        log.push((micros(p.clock().wall()), model.clone()));
    }
    log
}

/// `time` to the microsecond, as commit times are kept.
fn micros(time: SystemTime) -> SystemTime {
    let us = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    SystemTime::UNIX_EPOCH + Duration::from_micros(us)
}

/// The state at `time` by the log: the last commit's at or before it.
fn expected(log: &[(SystemTime, Model)], time: SystemTime) -> Option<&Model> {
    log.iter().rev().find(|(t, _)| *t <= time).map(|(_, m)| m)
}

#[test]
fn a_time_reads_the_last_commit_at_or_before_it() {
    harness::run("txn_time_travel", 4, |seed| {
        let p = Arc::new(SimPlatform::new(seed));
        let e = open(&p, Duration::from_secs(24 * 3600)).unwrap();
        let rng = SplitMix64::new(seed);
        let log = workload(&p, &e, &rng, 120);
        let (first, last) = (log[0].0, log.last().unwrap().0);
        let span = last.duration_since(first).unwrap().as_micros() as u64;
        for _ in 0..300 {
            // A time within the log, or exactly one of its commits'.
            let time = if rng.below(3) == 0 {
                log[rng.below(log.len() as u64) as usize].0
            } else {
                first + Duration::from_micros(rng.below(span + 1))
            };
            let ts = e.snapshot_at(time).unwrap();
            assert_eq!(&read_at(&e, ts).unwrap(), expected(&log, time).unwrap());
        }
        // Before the first commit: unknown.
        assert!(matches!(
            e.snapshot_at(first - Duration::from_micros(1)),
            Err(Error::SnapshotTooOld)
        ));
        // Past the last: the newest snapshot.
        assert_eq!(
            e.snapshot_at(last + Duration::from_secs(60)).unwrap(),
            e.visible()
        );
        // Each commit's time, as recorded.
        let ts = e.snapshot_at(last).unwrap();
        let us = e.commit_time(ts).unwrap().unwrap();
        assert_eq!(
            SystemTime::UNIX_EPOCH + Duration::from_micros(us),
            last,
            "the last commit's time"
        );
    });
}

#[test]
fn the_retention_keeps_what_it_covers_and_no_more() {
    let p = Arc::new(SimPlatform::new(7));
    let e = open(&p, Duration::from_secs(60)).unwrap();
    let rng = SplitMix64::new(7);
    let log = workload(&p, &e, &rng, 200);
    e.vacuum().unwrap();
    let now = micros(p.clock().wall());
    let cutoff = now - Duration::from_secs(60);
    // Within the retention: every time reads its state.
    // (Of commits within one microsecond, a time reads the last.)
    for (time, _) in log.iter().filter(|(t, _)| *t >= cutoff) {
        let ts = e.snapshot_at(*time).unwrap();
        assert_eq!(&read_at(&e, ts).unwrap(), expected(&log, *time).unwrap());
    }
    // How far back: at or before the cutoff, not before the commit at it.
    let (oldest, at) = e.reach().unwrap().unwrap();
    let at = SystemTime::UNIX_EPOCH + Duration::from_micros(at);
    assert!(at <= cutoff, "{at:?} after the cutoff {cutoff:?}");
    assert_eq!(&read_at(&e, oldest).unwrap(), expected(&log, at).unwrap());
    // Long past it: the versions are gone (and the times with them).
    let old = log[0].0;
    if old < at {
        let gone = e.snapshot_at(old).and_then(|ts| read_at(&e, ts));
        assert!(matches!(gone, Err(Error::SnapshotTooOld)), "{gone:?}");
    }
    // Versions kept: about those of the last minute, not of all 200.
    let versions = e.version_count().unwrap();
    let within = log.iter().filter(|(t, _)| *t >= cutoff).count() as u64;
    assert!(
        versions <= 30 + 4 * within,
        "{versions} versions, {within} commits within"
    );
    // Time passing alone frees the rest.
    p.clock().sleep(Duration::from_secs(120));
    let mut t = e.begin();
    t.put(b"tick", b"1").unwrap();
    t.commit().unwrap();
    p.clock().sleep(Duration::from_secs(2));
    e.vacuum().unwrap();
    // The oldest reachable is now the workload's last commit — the last
    // at or before the cutoff, whose state the times since it read.
    let (_, at2) = e.reach().unwrap().unwrap();
    assert_eq!(
        SystemTime::UNIX_EPOCH + Duration::from_micros(at2),
        log.last().unwrap().0
    );
    assert!(matches!(
        e.snapshot_at(log[150].0),
        Err(Error::SnapshotTooOld)
    ));
    assert!(e.version_count().unwrap() <= 31 + 1);
}

#[test]
fn without_a_retention_by_time_nothing_is_recorded() {
    let p = Arc::new(SimPlatform::new(3));
    let e = open(&p, Duration::ZERO).unwrap();
    let rng = SplitMix64::new(3);
    let log = workload(&p, &e, &rng, 20);
    assert!(matches!(
        e.snapshot_at(log[10].0),
        Err(Error::SnapshotTooOld)
    ));
    assert_eq!(e.reach().unwrap(), None);
    assert_eq!(e.commit_time(e.visible()).unwrap(), None);
    // Only the keys' versions in the store.
    e.vacuum().unwrap();
    assert_eq!(
        e.version_count().unwrap(),
        log.last().unwrap().1.len() as u64
    );
}

#[test]
fn commit_times_survive_a_crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("txn_time_crash", 2, |seed| {
            let run = |p: &Arc<SimPlatform>| -> Vec<(SystemTime, Model)> {
                let rng = SplitMix64::new(seed);
                let mut log = Vec::new();
                let _ = (|| -> Result<(), Error> {
                    let e = open(p, Duration::from_secs(3600))?;
                    let mut model = Model::new();
                    for i in 0..30 {
                        p.clock().sleep(Duration::from_millis(1 + rng.below(3000)));
                        let mut t = e.begin();
                        let k = format!("k{}", rng.below(8)).into_bytes();
                        t.put(&k, format!("{i}").as_bytes())?;
                        model.insert(k, format!("{i}").into_bytes());
                        t.commit()?;
                        log.push((micros(p.clock().wall()), model.clone()));
                        if i % 7 == 6 {
                            e.vacuum()?;
                        }
                    }
                    Ok(())
                })();
                log
            };
            let dry = Arc::new(SimPlatform::new(seed));
            let full = run(&dry);
            let total = dry.sim_fs().op_count();
            for n in 1..=total {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let acked = run(&p);
                let e = open(&p, Duration::from_secs(3600))
                    .unwrap_or_else(|err| panic!("{mode:?} op {n}: {err}"));
                let k: usize = e.visible().to_string().parse().unwrap();
                assert!(k >= acked.len(), "{mode:?} op {n}");
                // Every commit that survived reads by its time.
                for (time, _) in &full[..k] {
                    let ts = e.snapshot_at(*time).unwrap();
                    let want = expected(&full[..k], *time).unwrap();
                    assert_eq!(&read_at(&e, ts).unwrap(), want, "{mode:?} op {n}");
                }
                // And a commit after the crash comes after them in time,
                // however the clock stands.
                let mut t = e.begin();
                t.put(b"after", b"1").unwrap();
                let ts = t.commit().unwrap();
                let us = e.commit_time(ts).unwrap().unwrap();
                if k > 0 {
                    let before = e
                        .commit_time(k.to_string().parse::<Timestamp>().unwrap())
                        .unwrap()
                        .unwrap();
                    assert!(us >= before, "{mode:?} op {n}");
                }
            }
        });
    }
}
