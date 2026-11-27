//! Streams in the engine (PLAN.md §M16.5 S1, D23): appends numbered by
//! the commit — dense, in commit order, never reused —, their time, the
//! head, savepoints, waiting, crashes.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Error, Transaction, stream_key};

/// A stream named `s`: its head `s/h`, its entries under `s/e`.
fn head_key(s: &[u8]) -> Vec<u8> {
    [s, b"/h"].concat()
}

fn entries_key(s: &[u8]) -> Vec<u8> {
    [s, b"/e"].concat()
}

/// The value as appended: 16 bytes the commit fills (number, time),
/// then the bytes.
fn fill(v: &mut [u8], n: u64, time: u64) {
    v[..8].copy_from_slice(&n.to_be_bytes());
    v[8..16].copy_from_slice(&time.to_be_bytes());
}

fn add(t: &mut Transaction, s: &[u8], v: &[u8]) -> Result<(), Error> {
    t.append(
        &head_key(s),
        &entries_key(s),
        &[&[0u8; 16][..], v].concat(),
        fill,
    )
}

fn opts() -> EngineOptions {
    EngineOptions {
        store: StoreOptions {
            cache_pages: 32,
            wal_segment_size: 4096,
            ..Default::default()
        },
        checkpoint_wal_bytes: 16 << 10,
        history: 0,
        history_time: 0,
        txn_memory: 64 << 20,
        wal_minimal: true,
    }
}

fn open(p: &Arc<SimPlatform>) -> Result<Engine, Error> {
    Engine::open(Arc::clone(p) as Arc<dyn Platform>, Path::new("/db"), opts())
}

/// A stream's entries a transaction sees: (number, time, bytes).
fn entries(t: &Transaction, stream: &[u8]) -> Vec<(u64, u64, Vec<u8>)> {
    let p = entries_key(stream);
    let start = stream_key(&p, 0);
    let end = stream_key(&p, u64::MAX >> 1);
    t.scan(Bound::Included(&start), Bound::Included(&end))
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            let n = u64::from_be_bytes(v[..8].try_into().unwrap());
            // The key's number is the value's.
            assert_eq!(k, stream_key(&p, n));
            let time = u64::from_be_bytes(v[8..16].try_into().unwrap());
            (n, time, v[16..].to_vec())
        })
        .collect()
}

fn head(t: &Transaction, stream: &[u8]) -> Option<u64> {
    t.get(&head_key(stream)).unwrap().map(|v| {
        assert_eq!(v.len(), 16, "a head: the last number and its time");
        u64::from_be_bytes(v[..8].try_into().unwrap())
    })
}

#[test]
fn appends_are_numbered_by_the_commit() {
    let p = Arc::new(SimPlatform::new(1));
    let e = open(&p).unwrap();
    // Two transactions append, the later-begun one commits first: its
    // entries come first.
    let mut a = e.begin();
    let mut b = e.begin();
    add(&mut a, b"s", b"a1").unwrap();
    add(&mut a, b"s", b"a2").unwrap();
    add(&mut b, b"s", b"b1").unwrap();
    add(&mut b, b"t", b"other").unwrap();
    // Not read back before the commit.
    assert!(entries(&a, b"s").is_empty());
    b.commit().unwrap();
    a.commit().unwrap();
    let t = e.begin();
    let got = entries(&t, b"s");
    let bytes: Vec<&[u8]> = got.iter().map(|(_, _, v)| v.as_slice()).collect();
    assert_eq!(bytes, [&b"b1"[..], b"a1", b"a2"]);
    assert_eq!(got.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(got.windows(2).all(|w| w[0].1 <= w[1].1), "times in order");
    assert_eq!(head(&t, b"s"), Some(3));
    assert_eq!(entries(&t, b"t").len(), 1);
    assert_eq!(head(&t, b"t"), Some(1));
    // Another stream's head is not touched.
    assert_eq!(head(&t, b"u"), None);
    // Number 1's key sorts after a smaller prefix's own keys: entries
    // stay inside their range.
    assert!(stream_key(b"s/e", 1) > b"s/e".to_vec());
}

#[test]
fn rollbacks_and_savepoints_take_appends_back() {
    let p = Arc::new(SimPlatform::new(2));
    let e = open(&p).unwrap();
    let mut t = e.begin();
    add(&mut t, b"s", b"gone").unwrap();
    t.rollback();
    let mut t = e.begin();
    add(&mut t, b"s", b"kept").unwrap();
    t.savepoint().unwrap();
    add(&mut t, b"s", b"undone").unwrap();
    t.rollback_to_savepoint().unwrap();
    add(&mut t, b"s", b"after").unwrap();
    t.savepoint().unwrap();
    add(&mut t, b"s", b"released").unwrap();
    t.release_savepoint();
    t.commit().unwrap();
    let got = entries(&e.begin(), b"s");
    let bytes: Vec<&[u8]> = got.iter().map(|(_, _, v)| v.as_slice()).collect();
    // No number for what was taken back: dense from 1.
    assert_eq!(bytes, [&b"kept"[..], b"after", b"released"]);
    assert_eq!(got.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2, 3]);
    // A read-only transaction appends nothing.
    let mut r = e
        .begin_with(nucleus_txn::TxnOptions {
            read_only: true,
            ..Default::default()
        })
        .unwrap();
    assert!(matches!(add(&mut r, b"s", b"x"), Err(Error::ReadOnly)));
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn appenders_never_conflict_and_no_number_is_lost() {
    let p = Arc::new(SimPlatform::new(3));
    let e = open(&p).unwrap();
    let threads = 4;
    let per = 50;
    std::thread::scope(|s| {
        for w in 0..threads {
            let e = &e;
            s.spawn(move || {
                for i in 0..per {
                    let mut t = e.begin();
                    add(&mut t, b"s", format!("{w}:{i}").as_bytes()).unwrap();
                    // A write of its own too, as a table's with its event.
                    t.put(format!("row{w}").as_bytes(), b"x").unwrap();
                    t.commit().expect("appends do not conflict");
                }
            });
        }
    });
    let got = entries(&e.begin(), b"s");
    assert_eq!(got.len(), threads * per);
    assert!(got.iter().enumerate().all(|(i, x)| x.0 == i as u64 + 1));
    // Each writer's entries in its own order.
    for w in 0..threads {
        let mine: Vec<usize> = got
            .iter()
            .filter_map(|(_, _, v)| {
                let v = String::from_utf8(v.clone()).unwrap();
                let (a, b) = v.split_once(':').unwrap();
                (a == w.to_string()).then(|| b.parse().unwrap())
            })
            .collect();
        assert_eq!(mine, (0..per).collect::<Vec<_>>());
    }
}

#[test]
fn numbers_go_on_after_reopening_and_trimming() {
    let p = Arc::new(SimPlatform::new(4));
    let e = open(&p).unwrap();
    for i in 0..5u8 {
        let mut t = e.begin();
        add(&mut t, b"s", &[i]).unwrap();
        t.commit().unwrap();
    }
    // Trimmed whole: the head stays, so numbers are not reused.
    let mut t = e.begin();
    for n in 1..=5 {
        t.delete(&stream_key(&entries_key(b"s"), n)).unwrap();
    }
    t.commit().unwrap();
    drop(e);
    p.sim_fs().crash();
    let e = open(&p).unwrap();
    let mut t = e.begin();
    add(&mut t, b"s", b"next").unwrap();
    t.commit().unwrap();
    let got = entries(&e.begin(), b"s");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 6);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_waiter_wakes_when_an_entry_is_visible() {
    let p = Arc::new(SimPlatform::new(5));
    let e = open(&p).unwrap();
    assert_eq!(
        e.wait_appended(&head_key(b"s"), 0, Duration::ZERO).unwrap(),
        0
    );
    std::thread::scope(|s| {
        let waiter = s.spawn(|| {
            let began = std::time::Instant::now();
            let n = e
                .wait_appended(&head_key(b"s"), 0, Duration::from_secs(20))
                .unwrap();
            // Woken by the commit, not by the timeout.
            assert!(began.elapsed() < Duration::from_secs(10));
            n
        });
        std::thread::sleep(Duration::from_millis(50));
        let mut t = e.begin();
        add(&mut t, b"s", b"x").unwrap();
        add(&mut t, b"s", b"y").unwrap();
        t.commit().unwrap();
        let n = waiter.join().unwrap();
        assert_eq!(n, 2);
        // What it was told is there to read.
        assert_eq!(entries(&e.begin(), b"s").len(), 2);
    });
    // Past the last: waits out the timeout and says where the stream is.
    assert_eq!(
        e.wait_appended(&head_key(b"s"), 2, Duration::from_millis(10))
            .unwrap(),
        2
    );
    // Another stream's appends do not answer it.
    let mut t = e.begin();
    add(&mut t, b"other", b"x").unwrap();
    t.commit().unwrap();
    assert_eq!(
        e.wait_appended(&head_key(b"s"), 2, Duration::ZERO).unwrap(),
        2
    );
}

#[test]
fn appends_survive_a_crash_at_every_io_point() {
    // Each commit appends a few entries to two streams and writes a key:
    // after a crash anywhere, the streams hold exactly the commits made
    // durable — dense, in order — and appending goes on from there.
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("txn_stream_crash", 2, |seed| {
            let run = |p: &Arc<SimPlatform>| -> (Vec<Vec<Vec<u8>>>, usize) {
                let rng = SplitMix64::new(seed);
                let mut states = vec![Vec::new()];
                let mut model: Vec<Vec<u8>> = Vec::new();
                let mut acked = 0;
                let _ = (|| -> Result<(), Error> {
                    let e = open(p)?;
                    for i in 1..=20u8 {
                        let mut t = e.begin();
                        t.put(b"m", &[i])?;
                        for j in 0..1 + rng.below(4) as u8 {
                            let v = vec![i, j];
                            add(&mut t, b"s", &v)?;
                            model.push(v);
                            add(&mut t, b"z", &[i])?;
                        }
                        states.push(model.clone());
                        t.commit()?;
                        acked = i as usize;
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
                let e = open(&p).unwrap_or_else(|err| panic!("{mode:?} op {n}: {err}"));
                let t = e.begin();
                let k = t.get(b"m").unwrap().map_or(0, |v| v[0] as usize);
                assert!(k >= acked, "{mode:?} op {n}: at {k}, acked {acked}");
                let got = entries(&t, b"s");
                let want = &states[k];
                assert_eq!(
                    got.iter().map(|x| x.2.clone()).collect::<Vec<_>>(),
                    *want,
                    "{mode:?} op {n}"
                );
                assert!(got.iter().enumerate().all(|(i, x)| x.0 == i as u64 + 1));
                assert_eq!(head(&t, b"s").unwrap_or(0), want.len() as u64);
                drop(t);
                let mut t = e.begin();
                add(&mut t, b"s", b"after").unwrap();
                t.commit().unwrap();
                let last = entries(&e.begin(), b"s").pop().unwrap();
                assert_eq!(last.0, want.len() as u64 + 1, "{mode:?} op {n}");
            }
        });
    }
}

#[test]
fn an_entry_s_time_is_never_before_the_last_one_s() {
    // A head whose last entry is from the future (a clock that stepped
    // back since, a restart on another machine): the next entries take
    // that time, not the clock's.
    let p = Arc::new(SimPlatform::new(6));
    let e = open(&p).unwrap();
    let future = u64::MAX / 2;
    let mut t = e.begin();
    let mut head = 7u64.to_be_bytes().to_vec();
    head.extend_from_slice(&future.to_be_bytes());
    t.put(&head_key(b"s"), &head).unwrap();
    t.commit().unwrap();
    let mut t = e.begin();
    add(&mut t, b"s", b"x").unwrap();
    t.commit().unwrap();
    let got = entries(&e.begin(), b"s");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 8);
    assert_eq!(got[0].1, future);
}
