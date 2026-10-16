//! Pager tests: format, validation, the copy-on-write rule, the free list,
//! and crashes at every I/O point.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_platform::{OpenMode, Platform, Rng, SplitMix64};
use nucleus_storage::pager::{FIRST_DATA_PAGE, IDS_PER_FREELIST_PAGE};
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

fn raw(p: &SimPlatform, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    p.fs()
        .open(Path::new(DB), OpenMode::ReadOnly)
        .unwrap()
        .read_exact_at(&mut buf, offset)
        .unwrap();
    buf
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
    pager.set_meta(3, 0xABCD).unwrap();
    pager.close().unwrap();

    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(pager.meta(3), 0xABCD);
    assert_eq!(pager.meta(0), 0);
    for (i, id) in ids.iter().enumerate() {
        assert!(same_payload(
            &pager.read_page(*id).unwrap(),
            &page_with(i as u64)
        ));
    }
    assert_eq!(pager.page_count(), 7);
    pager.check().unwrap();
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
fn durable_pages_are_immutable() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let id = pager.allocate().unwrap();
    assert!(pager.is_writable(id));
    pager.write_page(id, &mut page_with(1)).unwrap();
    pager.write_page(id, &mut page_with(2)).unwrap(); // same epoch: in place
    pager.sync().unwrap();
    assert!(!pager.is_writable(id));
    assert!(matches!(
        pager.write_page(id, &mut page_with(3)),
        Err(Error::Immutable(_))
    ));
    assert!(same_payload(&pager.read_page(id).unwrap(), &page_with(2)));
}

#[test]
fn detects_a_flipped_bit() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let id = pager.allocate().unwrap();
    pager.write_page(id, &mut page_with(1)).unwrap();
    pager.sync().unwrap();
    let at = id.offset() + 5000;
    let byte = raw(&p, at, 1)[0];
    poke(&p, at, &[byte ^ 0x10]);
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
    poke(&p, b.offset(), &raw(&p, a.offset(), PAGE_SIZE)); // a's bytes where b belongs
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
        pager.write_page(id, &mut Page::new(PageType::FREELIST)),
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
    pager.set_meta(0, 1234).unwrap();
    pager.sync().unwrap(); // seq 2 -> slot 0
    pager.set_meta(0, 5678).unwrap();
    pager.sync().unwrap(); // seq 3 -> slot 1
    drop(pager);

    poke(&p, PAGE_SIZE as u64 + 100, &[0xFF; 8]); // damage slot 1, the newer
    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    assert_eq!(pager.meta(0), 1234); // fell back to the older copy
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
fn freed_pages_are_reused_after_the_next_sync() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let ids: Vec<PageId> = (0..10).map(|_| pager.allocate().unwrap()).collect();
    for id in &ids {
        pager.write_page(*id, &mut page_with(id.0)).unwrap();
    }
    pager.sync().unwrap();

    // Freed durable pages wait for the next commit...
    for id in &ids[..5] {
        pager.free(*id).unwrap();
    }
    assert_eq!(pager.allocate().unwrap(), PageId(12));
    assert!(matches!(pager.free(ids[0]), Err(Error::InvalidPage(_)))); // double free
    pager.sync().unwrap();
    // ...and come back, lowest first (one went to hold the free list).
    let again: BTreeSet<PageId> = (0..4).map(|_| pager.allocate().unwrap()).collect();
    assert!(again.iter().all(|id| ids[..5].contains(id)), "{again:?}");

    // A page allocated and freed within one epoch is reusable at once.
    let fresh = pager.allocate().unwrap();
    pager.free(fresh).unwrap();
    assert_eq!(pager.allocate().unwrap(), fresh);
    pager.check().unwrap();
}

#[test]
fn a_long_free_list_spans_pages() {
    let p = platform(1);
    let pager = Pager::create(p.fs(), Path::new(DB)).unwrap();
    let n = IDS_PER_FREELIST_PAGE * 2 + 10;
    let ids: Vec<PageId> = (0..n).map(|_| pager.allocate().unwrap()).collect();
    pager.sync().unwrap();
    for id in &ids {
        pager.free(*id).unwrap();
    }
    pager.sync().unwrap();
    pager.check().unwrap();
    let count = pager.page_count();
    drop(pager);
    let pager = Pager::open(p.fs(), Path::new(DB)).unwrap();
    pager.check().unwrap();
    assert_eq!(pager.page_count(), count);
    // Every page but the headers and the free list's own pages is free.
    let acc = pager.accounting();
    assert_eq!(
        acc.free.len() + acc.freelist_pages.len(),
        (count - 2) as usize
    );
    assert_eq!(acc.freelist_pages.len(), 3);
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
        pager.set_meta(0, 1).unwrap();
        // Overwriting raw, as a buggy writer would: the pager must notice.
        poke_unsynced(&p, id.offset(), page_bytes(&p, id, 2));
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

/// The sealed bytes `page_with(tag)` would have at `id`.
fn page_bytes(p: &SimPlatform, id: PageId, tag: u64) -> Vec<u8> {
    // Seal via a scratch pager so the bytes carry a valid checksum and id.
    let scratch = "/db/scratch";
    let _ = p.fs().remove_file(Path::new(scratch));
    let pager = Pager::create(p.fs(), Path::new(scratch)).unwrap();
    while pager.allocate().unwrap() < id {}
    pager.write_page(id, &mut page_with(tag)).unwrap();
    let f = p.fs().open(Path::new(scratch), OpenMode::ReadOnly).unwrap();
    let mut buf = vec![0u8; PAGE_SIZE];
    f.read_exact_at(&mut buf, id.offset()).unwrap();
    buf
}

fn poke_unsynced(p: &SimPlatform, offset: u64, bytes: Vec<u8>) {
    let f = p.fs().open(Path::new(DB), OpenMode::ReadWrite).unwrap();
    f.write_all_at(&bytes, offset).unwrap();
}

/// What a committed state promises: its meta, its page count, and the
/// contents of every live page.
#[derive(Clone, Debug)]
struct Snapshot {
    epoch: u64,
    page_count: u64,
    live: BTreeMap<u64, u64>, // page -> content tag
}

/// A random workload of allocations, writes, copy-on-write rewrites and
/// frees, with a sync per epoch.  Stops at the first error (the crash) and
/// returns the snapshots of the syncs that completed.
fn workload(p: &SimPlatform, seed: u64) -> Vec<Snapshot> {
    let rng = SplitMix64::new(seed);
    let mut snaps = Vec::new();
    let _ = (|| -> Result<(), Error> {
        let pager = Pager::create(p.fs(), Path::new(DB))?;
        let mut live: BTreeMap<u64, u64> = BTreeMap::new();
        snaps.push(Snapshot {
            epoch: 0,
            page_count: 2,
            live: live.clone(),
        });
        for epoch in 1..=6 {
            // Epoch 2 allocates in bulk and epoch 3 frees in bulk, so the
            // free list is long and reused within later epochs.
            let (steps, forced) = match epoch {
                2 => (40, Some(0)),
                3 => (36, Some(6)),
                _ => (10, None),
            };
            for step in 0..steps {
                let tag = epoch * 1000 + step;
                let choice = forced.unwrap_or_else(|| rng.below(10));
                if choice < 5 || live.is_empty() {
                    let id = pager.allocate()?;
                    pager.write_page(id, &mut page_with(tag))?;
                    live.insert(id.0, tag);
                    continue;
                }
                let keys: Vec<u64> = live.keys().copied().collect();
                let id = keys[rng.below(keys.len() as u64) as usize];
                if choice < 8 {
                    pager.free(PageId(id))?;
                    live.remove(&id);
                } else if pager.is_writable(PageId(id)) {
                    pager.write_page(PageId(id), &mut page_with(tag))?;
                    live.insert(id, tag);
                } else {
                    // Copy-on-write: a new page, the old one freed.
                    let new = pager.allocate()?;
                    pager.write_page(new, &mut page_with(tag))?;
                    pager.free(PageId(id))?;
                    live.remove(&id);
                    live.insert(new.0, tag);
                }
            }
            pager.set_meta(0, epoch)?;
            pager.sync()?;
            snaps.push(Snapshot {
                epoch,
                page_count: pager.page_count(),
                live: live.clone(),
            });
        }
        pager.close()
    })();
    snaps
}

#[test]
fn crash_at_every_io_point() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("crash_at_every_io_point", 4, |seed| {
            // A dry run counts the operations; then crash at each one.
            let dry = platform(seed);
            let before = dry.sim_fs().op_count();
            let snaps = workload(&dry, seed);
            assert_eq!(snaps.len(), 7);
            let total = dry.sim_fs().op_count() - before;

            for n in 1..=total {
                let p = platform(seed);
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let snaps = workload(&p, seed);
                assert_eq!(
                    p.sim_fs().crash_count(),
                    1,
                    "op {n} of {total} did not crash"
                );
                check_after_crash(&p, &snaps, n);
            }
        });
    }
}

fn check_after_crash(p: &SimPlatform, snaps: &[Snapshot], n: u64) {
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
    let epoch = pager.meta(0);
    let last = snaps.last().map_or(0, |s| s.epoch);
    assert!(
        epoch == last || epoch == last + 1,
        "{ctx}: epoch {epoch}, last sync {last}"
    );
    let Some(snap) = snaps.iter().find(|s| s.epoch == epoch) else {
        return; // the in-flight sync won; its snapshot was never recorded
    };
    assert_eq!(pager.page_count(), snap.page_count, "{ctx}: page count");
    // Every live page holds its contents, and every page is accounted for:
    // live, free, or holding the free list.  Nothing leaks.
    for (&id, &tag) in &snap.live {
        let page = pager
            .read_page(PageId(id))
            .unwrap_or_else(|e| panic!("{ctx}: page {id}: {e}"));
        assert!(
            same_payload(&page, &page_with(tag)),
            "{ctx}: page {id} has wrong contents"
        );
    }
    let acc = pager.accounting();
    let mut all: BTreeSet<u64> = snap.live.keys().copied().collect();
    for id in acc.free.iter().chain(&acc.freelist_pages) {
        assert!(all.insert(*id), "{ctx}: page {id} both live and free");
    }
    assert_eq!(all.len() as u64, snap.page_count - 2, "{ctx}: pages leaked");
}
