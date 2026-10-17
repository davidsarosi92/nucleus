//! The simulation's own guarantees: durability, crashes, faults, determinism.

use std::path::Path;
use std::time::Duration;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SECTOR_SIZE, SimFault, SimPlatform, sim_fault};
use nucleus_platform::{File, OpenMode, Platform};
use proptest::prelude::*;

fn read_all(f: &dyn File) -> Vec<u8> {
    let mut buf = vec![0u8; f.size().unwrap() as usize];
    f.read_exact_at(&mut buf, 0).unwrap();
    buf
}

fn read_path(p: &SimPlatform, path: &str) -> Option<Vec<u8>> {
    let f = p.fs().open(Path::new(path), OpenMode::ReadOnly).ok()?;
    Some(read_all(f.as_ref()))
}

/// `/db` exists durably, `/db/f` exists durably and holds `content`.
fn setup(p: &SimPlatform, content: &[u8]) -> Box<dyn File> {
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    p.fs().sync_dir(Path::new("/")).unwrap();
    let f = p.fs().open(Path::new("/db/f"), OpenMode::Create).unwrap();
    f.write_all_at(content, 0).unwrap();
    f.sync().unwrap();
    p.fs().sync_dir(Path::new("/db")).unwrap();
    f
}

#[test]
fn crash_keeps_synced_and_drops_unsynced_writes() {
    harness::run("crash_keeps_synced_and_drops_unsynced_writes", 16, |seed| {
        let p = SimPlatform::new(seed);
        let f = setup(&p, &[1u8; 4096]);
        f.write_all_at(&[2u8; 4096], 0).unwrap(); // eight sectors, none synced
        f.write_all_at(&[3u8; 100], 4096).unwrap(); // and growth
        assert_eq!(f.size().unwrap(), 4196);
        p.sim_fs().crash();
        assert_eq!(read_path(&p, "/db/f").unwrap(), vec![1u8; 4096]);
    });
}

#[test]
fn handles_go_stale_after_a_crash() {
    let p = SimPlatform::new(1);
    let f = setup(&p, b"x");
    p.sim_fs().crash();
    let err = f.size().unwrap_err();
    assert_eq!(sim_fault(&err), Some(SimFault::StaleHandle));
    // A new "process" opens it again.
    assert!(read_path(&p, "/db/f").is_some());
}

#[test]
fn a_new_file_needs_its_directory_synced() {
    let p = SimPlatform::new(1);
    setup(&p, b"x");
    let create = |sync_dir: bool| {
        let f = p.fs().open(Path::new("/db/new"), OpenMode::Create).unwrap();
        f.write_all_at(b"data", 0).unwrap();
        f.sync().unwrap(); // the data is durable either way...
        if sync_dir {
            p.fs().sync_dir(Path::new("/db")).unwrap();
        }
        p.sim_fs().crash();
    };
    create(false);
    assert_eq!(read_path(&p, "/db/new"), None); // ...but nothing names it
    create(true);
    assert_eq!(read_path(&p, "/db/new").unwrap(), b"data");
}

#[test]
fn a_directory_needs_its_parent_synced() {
    let p = SimPlatform::new(1);
    p.fs().create_dir_all(Path::new("/a/b")).unwrap();
    p.fs().sync_dir(Path::new("/a")).unwrap(); // b is listed in a, but a is not in /
    p.sim_fs().crash();
    assert!(!p.fs().exists(Path::new("/a")).unwrap());
    assert!(!p.fs().exists(Path::new("/a/b")).unwrap());
}

#[test]
fn rename_is_durable_only_after_sync_dir() {
    let p = SimPlatform::new(1);
    setup(&p, b"old");
    let tmp = p.fs().open(Path::new("/db/tmp"), OpenMode::Create).unwrap();
    tmp.write_all_at(b"new", 0).unwrap();
    tmp.sync().unwrap();
    p.fs()
        .rename(Path::new("/db/tmp"), Path::new("/db/f"))
        .unwrap();
    p.sim_fs().crash();
    assert_eq!(read_path(&p, "/db/f").unwrap(), b"old");

    let tmp = p.fs().open(Path::new("/db/tmp"), OpenMode::Create).unwrap();
    tmp.write_all_at(b"new", 0).unwrap();
    tmp.sync().unwrap();
    p.fs()
        .rename(Path::new("/db/tmp"), Path::new("/db/f"))
        .unwrap();
    p.fs().sync_dir(Path::new("/db")).unwrap();
    p.sim_fs().crash();
    assert_eq!(read_path(&p, "/db/f").unwrap(), b"new");
    assert_eq!(read_path(&p, "/db/tmp"), None);
}

#[test]
fn a_failed_sync_loses_the_writes_for_good() {
    let p = SimPlatform::new(1);
    let f = setup(&p, b"aaaa");
    f.write_all_at(b"bb", 0).unwrap();
    p.sim_fs().set_faults(Faults {
        sync_error_ppm: 1_000_000,
        ..Faults::default()
    });
    assert_eq!(sim_fault(&f.sync().unwrap_err()), Some(SimFault::Injected));
    p.sim_fs().set_faults(Faults::default());
    // The writes are still visible, and a later sync "succeeds"...
    assert_eq!(read_all(f.as_ref()), b"bbaa");
    f.sync().unwrap();
    // ...but they never reached the disk.
    p.sim_fs().crash();
    assert_eq!(read_path(&p, "/db/f").unwrap(), b"aaaa");
}

#[test]
fn injected_errors_change_nothing() {
    let p = SimPlatform::new(1);
    let f = setup(&p, b"abc");
    p.sim_fs().set_faults(Faults {
        read_error_ppm: 1_000_000,
        write_error_ppm: 1_000_000,
        ..Faults::default()
    });
    let mut buf = [0u8; 3];
    assert_eq!(
        sim_fault(&f.read_at(&mut buf, 0).unwrap_err()),
        Some(SimFault::Injected)
    );
    assert_eq!(
        sim_fault(&f.write_at(b"x", 0).unwrap_err()),
        Some(SimFault::Injected)
    );
    p.sim_fs().set_faults(Faults::default());
    assert_eq!(read_all(f.as_ref()), b"abc");
}

#[test]
fn scheduled_crash_fails_that_operation() {
    let p = SimPlatform::new(1);
    let f = setup(&p, b"abc");
    p.sim_fs().crash_after_ops(2);
    f.write_all_at(b"X", 0).unwrap(); // op 1
    let err = f.sync().unwrap_err(); // op 2: crashes instead
    assert_eq!(sim_fault(&err), Some(SimFault::Crash));
    assert_eq!(p.sim_fs().crash_count(), 1);
    assert_eq!(read_path(&p, "/db/f").unwrap(), b"abc");
}

#[test]
fn simulated_clock_moves_only_when_told() {
    let p = SimPlatform::new(1);
    assert_eq!(p.clock().now(), Duration::ZERO);
    p.sim_clock().advance(Duration::from_millis(1500));
    assert_eq!(p.clock().now(), Duration::from_millis(1500));
    assert_eq!(p.clock().now(), Duration::from_millis(1500));
    // Sleeping advances the clock instead of waiting.
    p.clock().sleep(Duration::from_secs(3600));
    assert_eq!(p.clock().now(), Duration::from_millis(3_601_500));
}

/// One scripted workload with torn crashes; returns everything observable.
fn torn_workload(seed: u64) -> (Vec<u64>, Option<Vec<u8>>, Vec<bool>) {
    let p = SimPlatform::new(seed);
    p.sim_fs().set_faults(Faults {
        crash_mode: CrashMode::Torn,
        ..Faults::default()
    });
    let f = setup(&p, &[1u8; 4096]);
    f.write_all_at(&[2u8; 3000], 100).unwrap();
    for i in 0..8 {
        drop(
            p.fs()
                .open(Path::new(&format!("/db/n{i}")), OpenMode::Create)
                .unwrap(),
        );
    }
    p.sim_fs().crash();
    let rng: Vec<u64> = (0..4).map(|_| p.rng().next_u64()).collect();
    let names = (0..8)
        .map(|i| p.fs().exists(Path::new(&format!("/db/n{i}"))).unwrap())
        .collect();
    (rng, read_path(&p, "/db/f"), names)
}

#[test]
fn same_seed_same_outcome() {
    harness::run("same_seed_same_outcome", 32, |seed| {
        assert_eq!(torn_workload(seed), torn_workload(seed));
    });
}

#[test]
fn different_seeds_differ() {
    let outcomes: std::collections::BTreeSet<_> = (0..16).map(torn_workload).collect();
    assert!(
        outcomes.len() > 1,
        "torn crashes did not depend on the seed"
    );
}

#[derive(Clone, Debug)]
enum Op {
    Write { offset: u16, len: u16, byte: u8 },
    Sync,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u16..4096, 1u16..1500, any::<u8>())
            .prop_map(|(offset, len, byte)| Op::Write { offset, len, byte }),
        1 => Just(Op::Sync),
    ]
}

proptest! {
    /// After a torn crash, every byte of the file holds either its value at
    /// the last sync or a value some later write put there — never anything
    /// else — and whatever was synced before is kept whole (as the length).
    #[test]
    fn torn_crash_only_mixes_old_and_new(ops in prop::collection::vec(op(), 1..40), seed: u64) {
        let p = SimPlatform::new(seed);
        p.sim_fs().set_faults(Faults { crash_mode: CrashMode::Torn, ..Faults::default() });
        let f = setup(&p, &[]);

        let mut synced: Vec<u8> = Vec::new();
        let mut current: Vec<u8> = Vec::new();
        let mut unsynced: Vec<(usize, Vec<u8>)> = Vec::new();
        for op in &ops {
            match *op {
                Op::Write { offset, len, byte } => {
                    let (offset, data) = (offset as usize, vec![byte; len as usize]);
                    f.write_all_at(&data, offset as u64).unwrap();
                    if current.len() < offset + data.len() {
                        current.resize(offset + data.len(), 0);
                    }
                    current[offset..offset + data.len()].copy_from_slice(&data);
                    unsynced.push((offset, data));
                }
                Op::Sync => {
                    f.sync().unwrap();
                    synced = current.clone();
                    unsynced.clear();
                }
            }
        }
        p.sim_fs().crash();
        let after = read_path(&p, "/db/f").unwrap();

        prop_assert!(after.len() >= synced.len());
        for (i, &b) in after.iter().enumerate() {
            let old = synced.get(i).copied().unwrap_or(0);
            let ok = b == old
                || unsynced.iter().any(|(o, d)| i >= *o && i < o + d.len() && d[i - o] == b);
            prop_assert!(ok, "byte {} = {} is neither old ({}) nor written", i, b, old);
        }
    }
}

#[test]
fn torn_writes_keep_or_lose_whole_sectors() {
    let sector = SECTOR_SIZE as usize;
    let (mut kept, mut lost) = (0, 0);
    harness::run("torn_writes_keep_or_lose_whole_sectors", 64, |seed| {
        let p = SimPlatform::new(seed);
        p.sim_fs().set_faults(Faults {
            crash_mode: CrashMode::Torn,
            ..Faults::default()
        });
        let f = setup(&p, &vec![0xAA; 8 * sector]);
        // Unaligned on purpose: starts mid-sector 1, ends mid-sector 6.
        f.write_all_at(&vec![0xBB; 5 * sector], (sector + 100) as u64)
            .unwrap();
        p.sim_fs().crash();
        let after = read_path(&p, "/db/f").unwrap();
        // Sectors 2..6 lie wholly inside the write.
        for s in 2..6 {
            let sec = &after[s * sector..(s + 1) * sector];
            assert!(
                sec.iter().all(|&b| b == sec[0]),
                "sector {s} is torn inside"
            );
            if sec[0] == 0xBB { kept += 1 } else { lost += 1 }
        }
    });
    assert!(
        kept > 0 && lost > 0,
        "torn crashes never mixed sectors: kept {kept}, lost {lost}"
    );
}
