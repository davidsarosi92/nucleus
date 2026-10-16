//! Pager tests: format, validation, free list, and crashes at every I/O point.

use std::collections::BTreeMap;
use std::path::Path;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{OpenMode, Platform, Rng, SplitMix64};
use nucleus_storage::pager::{FIRST_DATA_PAGE, RESERVE_TARGET};
use nucleus_storage::{Error, PAGE_SIZE, Page, PageId, PageType, Pager};

const DB: &str = "/db/data";
const USER: PageType = PageType::FIRST_USER;

fn platform(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(seed);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    p.fs().sync_dir(Path::new("/")).unwrap();
    p
}

fn page_with(tag: u64) -> Page {
    let mut page = Page::new(USER);
    let x = SplitMix64::new(tag);
    for chunk in page.payload_mut().chunks_mut(8) {
        let b = x.next_u64().to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    page
}

fn same_payload(a: &Page, b: &Page) -> bool {
    a.page_type() == b.page_type() && a.payload() == b.payload()
}

/// Overwrites raw bytes of the database file, behind the pager's back.
fn poke(p: &SimPlatform, offset: u64, bytes: &[u8]) {
    let f = p.fs().open(Path::new(DB), OpenMode::ReadWrite).unwrap();
    f.write_all_at(bytes, offset).unwrap();
    f.sync().unwrap();
}

#[test]
fn create_write_reopen() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let ids: Vec<PageId> = (0..5).map(|_| pager.allocate().unwrap()).collect();
    assert_eq!(ids[0], FIRST_DATA_PAGE);
    for (i, id) in ids.iter().enumerate() {
        pager.write_page(*id, &mut page_with(i as u64)).unwrap();
    }
    pager.set_root(3, Some(ids[2])).unwrap();
    pager.close().unwrap();

    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(pager.root(3), Some(ids[2]));
    assert_eq!(pager.root(0), None);
    for (i, id) in ids.iter().enumerate() {
        assert!(same_payload(
            &pager.read_page(*id).unwrap(),
            &page_with(i as u64)
        ));
    }
    assert_eq!(pager.page_count(), 7);
}

#[test]
fn works_on_the_real_file_system() {
    let dir = std::env::temp_dir().join(format!("nucleus-pager-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data");
    let real = nucleus_platform::real::RealPlatform::new().unwrap();
    let pager = Pager::create(real.fs(), &path).unwrap();
    let id = pager.allocate().unwrap();
    pager.write_page(id, &mut page_with(7)).unwrap();
    pager.close().unwrap();
    let pager = Pager::open(real.fs(), &path).unwrap();
    assert!(same_payload(&pager.read_page(id).unwrap(), &page_with(7)));
    drop(pager);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn create_refuses_an_existing_file() {
    let p = platform(1);
    Pager::create(p.fs(), Path::new(DB))
        .unwrap()
        .close()
        .unwrap();
    assert!(matches!(
        Pager::create(p.fs(), Path::new(DB)),
        Err(Error::Io(_))
    ));
}

#[test]
fn detects_a_flipped_bit() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let id = pager.allocate().unwrap();
    pager.write_page(id, &mut page_with(1)).unwrap();
    pager.sync().unwrap();
    let mut byte = [0u8];
    let at = id.offset() + 5000;
    p.fs()
        .open(Path::new(DB), OpenMode::ReadOnly)
        .unwrap()
        .read_exact_at(&mut byte, at)
        .unwrap();
    poke(&p, at, &[byte[0] ^ 0x10]);
    assert!(matches!(
        pager.read_page(id),
        Err(Error::Corrupt {
            reason: "checksum mismatch",
            ..
        })
    ));
}

#[test]
fn detects_a_misdirected_write() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let (a, b) = (pager.allocate().unwrap(), pager.allocate().unwrap());
    pager.write_page(a, &mut page_with(1)).unwrap();
    pager.write_page(b, &mut page_with(2)).unwrap();
    pager.sync().unwrap();
    // Page a's correct bytes, written where page b belongs.
    let mut raw = vec![0u8; PAGE_SIZE];
    p.fs()
        .open(Path::new(DB), OpenMode::ReadOnly)
        .unwrap()
        .read_exact_at(&mut raw, a.offset())
        .unwrap();
    poke(&p, b.offset(), &raw);
    assert!(matches!(
        pager.read_page(b),
        Err(Error::Corrupt {
            reason: "page id mismatch (misdirected write)",
            ..
        })
    ));
}

#[test]
fn reading_an_unwritten_page_is_an_error() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let id = pager.allocate().unwrap();
    assert!(matches!(pager.read_page(id), Err(Error::Corrupt { .. })));
    pager.sync().unwrap(); // now the page exists, as zeros
    assert!(matches!(
        pager.read_page(id),
        Err(Error::Corrupt {
            reason: "never written (all zeros)",
            ..
        })
    ));
    // The header counts the page, so the file must really be that long.
    drop(pager);
    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(pager.page_count(), id.0 + 1);
}

#[test]
fn refuses_invalid_and_reserved_pages() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    for id in [PageId(0), PageId(1), PageId(2), PageId(99)] {
        assert!(matches!(pager.read_page(id), Err(Error::InvalidPage(_))));
    }
    let id = pager.allocate().unwrap();
    // The pager's own page types are not for callers.
    assert!(matches!(
        pager.write_page(id, &mut Page::new(PageType::FREE)),
        Err(Error::InvalidPage(_))
    ));
}

#[test]
fn refuses_files_that_are_not_databases() {
    let p = platform(1);
    let f = p.fs().open(Path::new(DB), OpenMode::CreateNew).unwrap();
    f.write_all_at(&[0x42; 3 * PAGE_SIZE], 0).unwrap();
    assert!(matches!(
        Pager::open(p.fs(), Path::new(DB)),
        Err(Error::NotADatabase(_))
    ));

    let empty = "/db/empty";
    drop(p.fs().open(Path::new(empty), OpenMode::CreateNew).unwrap());
    assert!(matches!(
        Pager::open(p.fs(), Path::new(empty)),
        Err(Error::NotADatabase(_))
    ));
}

#[test]
fn survives_one_damaged_header_copy_not_two() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    pager.set_root(0, Some(PageId(1234))).unwrap();
    pager.sync().unwrap(); // seq 2 -> slot 0
    pager.set_root(0, Some(PageId(5678))).unwrap();
    pager.sync().unwrap(); // seq 3 -> slot 1
    drop(pager);

    poke(&p, PAGE_SIZE as u64 + 100, &[0xFF; 8]); // damage slot 1, the newer
    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(pager.root(0), Some(PageId(1234))); // fell back to the older copy
    drop(pager);

    poke(&p, 100, &[0xFF; 8]); // and slot 0
    poke(&p, PAGE_SIZE as u64 + 100, &[0xFF; 8]);
    assert!(matches!(
        Pager::open(p.fs(), Path::new(DB)),
        Err(Error::Corrupt {
            reason: "both header copies are damaged",
            ..
        })
    ));
}

#[test]
fn free_pages_are_reused() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let ids: Vec<PageId> = (0..10).map(|_| pager.allocate().unwrap()).collect();
    for id in &ids {
        pager.write_page(*id, &mut page_with(id.0)).unwrap();
    }
    pager.sync().unwrap();

    // Freed since the last sync: reused at once.
    for id in &ids[..5] {
        pager.free(*id).unwrap();
    }
    let again: Vec<PageId> = (0..5).map(|_| pager.allocate().unwrap()).collect();
    assert_eq!(sorted(&again), sorted(&ids[..5]));
    assert_eq!(pager.page_count(), 12);

    // Freed before the last sync: reused through the reserve.
    for id in &again {
        pager.free(*id).unwrap();
    }
    pager.sync().unwrap();
    assert_eq!((pager.free_count(), pager.reserved()), (0, 5));
    let from_reserve: Vec<PageId> = (0..5).map(|_| pager.allocate().unwrap()).collect();
    assert_eq!(sorted(&from_reserve), sorted(&ids[..5]));
    assert_eq!(pager.page_count(), 12);
    assert_eq!(pager.allocate().unwrap(), PageId(12)); // then the file grows
    pager.check().unwrap();
}

#[test]
fn close_returns_the_reserve() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let ids: Vec<PageId> = (0..RESERVE_TARGET as u64 + 8)
        .map(|_| pager.allocate().unwrap())
        .collect();
    for id in &ids {
        pager.write_page(*id, &mut page_with(0)).unwrap();
    }
    pager.sync().unwrap();
    for id in &ids {
        pager.free(*id).unwrap();
    }
    pager.sync().unwrap();
    assert_eq!(pager.reserved(), RESERVE_TARGET);
    assert_eq!(pager.free_count(), 8);
    pager.close().unwrap();

    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(
        pager.free_count() + pager.reserved() as u64,
        ids.len() as u64
    );
    pager.check().unwrap();
}

#[test]
fn a_failed_sync_poisons_the_pager() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let id = pager.allocate().unwrap();
    pager.write_page(id, &mut page_with(1)).unwrap();
    p.sim_fs().set_faults(Faults {
        sync_error_ppm: 1_000_000,
        ..Faults::default()
    });
    assert!(matches!(pager.sync(), Err(Error::Io(_))));
    p.sim_fs().set_faults(Faults::default());
    assert!(matches!(pager.sync(), Err(Error::Poisoned)));
    assert!(matches!(pager.read_page(id), Err(Error::Poisoned)));
    assert!(matches!(pager.allocate(), Err(Error::Poisoned)));
}

#[test]
fn torn_page_writes_are_detected() {
    let (mut old, mut new, mut caught) = (0, 0, 0);
    harness::run("torn_page_writes_are_detected", 64, |seed| {
        let p = platform(seed);
        p.sim_fs().set_faults(Faults {
            crash_mode: CrashMode::Torn,
            ..Faults::default()
        });
        let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
        let id = pager.allocate().unwrap();
        pager.write_page(id, &mut page_with(1)).unwrap();
        pager.sync().unwrap();
        pager.write_page(id, &mut page_with(2)).unwrap(); // never synced
        p.sim_fs().crash();

        let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
        match pager.read_page(id) {
            Ok(page) if same_payload(&page, &page_with(1)) => old += 1,
            Ok(page) if same_payload(&page, &page_with(2)) => new += 1,
            Ok(_) => panic!("a torn page passed verification"),
            Err(Error::Corrupt {
                reason: "checksum mismatch",
                ..
            }) => caught += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    });
    // Both likely outcomes happen, so the test exercises what it claims to.
    // (The whole new page surviving needs all 16 sectors to: 2^-16 a case.)
    assert!(old > 0 && caught > 0, "old {old}, new {new}, torn {caught}");
}

fn sorted(ids: &[PageId]) -> Vec<PageId> {
    let mut v = ids.to_vec();
    v.sort();
    v
}

/// What a committed state promises: its roots, its size, its free pages,
/// and the contents of every live page not touched after the commit.
#[derive(Clone, Debug)]
struct Snapshot {
    epoch: u64,
    page_count: u64,
    free_count: u64,
    live: BTreeMap<u64, u64>, // page -> content tag
}

/// A random workload of allocations, writes, rewrites and frees, with a
/// sync per epoch.  Stops at the first error (the crash).  Returns the
/// snapshots of the syncs that completed, and per page the epoch that last
/// touched it.
fn workload(p: &SimPlatform, seed: u64) -> (Vec<Snapshot>, BTreeMap<u64, u64>) {
    let rng = SplitMix64::new(seed);
    let mut snaps = Vec::new();
    let mut touched = BTreeMap::new();
    let _ = (|| -> Result<(), Error> {
        let pager = Pager::create(p.fs(), Path::new(DB))?;
        let mut live: BTreeMap<u64, u64> = BTreeMap::new();
        snaps.push(Snapshot {
            epoch: 0,
            page_count: 2,
            free_count: 0,
            live: live.clone(),
        });
        for epoch in 1..=6 {
            // Epoch 2 allocates in bulk and epoch 3 frees in bulk, so more
            // pages are free at a sync than the reserve takes: the durable
            // free list is non-empty, which is the case the allocation
            // rules exist for.
            let (steps, forced) = match epoch {
                2 => (RESERVE_TARGET as u64 + 12, Some(0)),
                3 => (RESERVE_TARGET as u64 + 8, Some(5)),
                _ => (8, None),
            };
            for step in 0..steps {
                let tag = epoch * 1000 + step;
                let choice = forced.unwrap_or_else(|| rng.below(10));
                if choice < 5 || live.is_empty() {
                    let id = pager.allocate()?;
                    touched.insert(id.0, epoch);
                    pager.write_page(id, &mut page_with(tag))?;
                    live.insert(id.0, tag);
                } else {
                    let keys: Vec<u64> = live.keys().copied().collect();
                    let id = keys[rng.below(keys.len() as u64) as usize];
                    touched.insert(id, epoch);
                    if choice < 8 {
                        pager.free(PageId(id))?;
                        live.remove(&id);
                    } else {
                        pager.write_page(PageId(id), &mut page_with(tag))?;
                        live.insert(id, tag);
                    }
                }
            }
            pager.set_root(0, Some(PageId(epoch)))?;
            pager.sync()?;
            snaps.push(Snapshot {
                epoch,
                page_count: pager.page_count(),
                free_count: pager.free_count() + pager.reserved() as u64,
                live: live.clone(),
            });
        }
        pager.close()
    })();
    (snaps, touched)
}

#[test]
fn crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("crash_at_every_io_point", 4, |seed| {
            // A dry run counts the operations; then crash at each one.
            let dry = platform(seed);
            let before = dry.sim_fs().op_count();
            workload(&dry, seed);
            let total = dry.sim_fs().op_count() - before;

            for n in 1..=total {
                let p = platform(seed);
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let (snaps, touched) = workload(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                check_after_crash(&p, &snaps, &touched, n);
            }
        });
    }
}

fn check_after_crash(p: &SimPlatform, snaps: &[Snapshot], touched: &BTreeMap<u64, u64>, n: u64) {
    let ctx = format!("crash at op {n}");
    let pager = match Pager::open(p.fs(), Path::new(DB)) {
        Ok(pager) => pager,
        // Crashed before `create` made the file durable: nothing to check.
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            assert!(snaps.is_empty(), "{ctx}: created file vanished");
            return;
        }
        Err(e) => panic!("{ctx}: reopen failed: {e}"),
    };
    pager
        .check()
        .unwrap_or_else(|e| panic!("{ctx}: check failed: {e}"));

    // The state is the last completed sync, or the one in flight.
    let epoch = pager.root(0).map_or(0, |r| r.0);
    let last = snaps.last().map_or(0, |s| s.epoch);
    assert!(
        epoch == last || epoch == last + 1,
        "{ctx}: epoch {epoch}, last sync {last}"
    );
    let Some(snap) = snaps.iter().find(|s| s.epoch == epoch) else {
        return; // the in-flight sync won; its snapshot was never recorded
    };
    assert_eq!(pager.page_count(), snap.page_count, "{ctx}: page count");
    // Leaks are allowed (at most one reserve's worth), losses are not.
    let free = pager.free_count() + pager.reserved() as u64;
    assert!(
        free <= snap.free_count && snap.free_count - free <= RESERVE_TARGET as u64,
        "{ctx}: free pages {free}, committed {}",
        snap.free_count
    );
    for (&id, &tag) in &snap.live {
        if touched.get(&id).is_some_and(|&e| e > epoch) {
            continue; // rewritten or freed later: the WAL's problem (§M4)
        }
        let page = pager
            .read_page(PageId(id))
            .unwrap_or_else(|e| panic!("{ctx}: page {id}: {e}"));
        assert!(
            same_payload(&page, &page_with(tag)),
            "{ctx}: page {id} has wrong contents"
        );
    }
}
