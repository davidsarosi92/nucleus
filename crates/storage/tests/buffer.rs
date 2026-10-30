//! Buffer pool tests: caching, eviction, pinning, the copy-on-write check,
//! and concurrent use.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use nucleus_platform::real::RealPlatform;
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Platform, Rng, SplitMix64};
use nucleus_storage::page::{read_u64_at, write_u64_at};
use nucleus_storage::{BufferPool, Error, PageId, PageType, Pager};

const USER: PageType = PageType::FIRST_USER;

fn pool(capacity: usize) -> (SimPlatform, BufferPool) {
    let p = SimPlatform::new(1);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    let pager = Arc::new(Pager::create(p.fs(), Path::new("/db/data")).unwrap());
    (p, BufferPool::new(pager, capacity))
}

/// Creates `n` pages, each holding its own id as a tag.
fn make_pages(pool: &BufferPool, n: usize) -> Vec<PageId> {
    (0..n)
        .map(|_| {
            let r = pool.create(USER).unwrap();
            let id = r.id();
            write_u64_at(r.write().unwrap().payload_mut(), 0, id.0);
            id
        })
        .collect()
}

fn tag(pool: &BufferPool, id: PageId) -> u64 {
    read_u64_at(pool.fetch(id).unwrap().read().payload(), 0)
}

#[test]
fn hits_and_misses() {
    let (_p, pool) = pool(4);
    let ids = make_pages(&pool, 2);
    assert_eq!(tag(&pool, ids[0]), ids[0].0);
    assert_eq!(tag(&pool, ids[1]), ids[1].0);
    let s = pool.stats();
    assert_eq!((s.hits, s.misses, s.evictions), (2, 0, 0));
}

#[test]
fn evicted_dirty_pages_are_written_back() {
    let (_p, pool) = pool(4);
    let ids = make_pages(&pool, 40); // ten times the capacity
    let s = pool.stats();
    assert!(s.evictions >= 36 && s.writebacks >= 36, "{s:?}");
    for id in &ids {
        assert_eq!(tag(&pool, *id), id.0);
    }
    assert!(pool.stats().misses >= 36);
}

#[test]
fn pinned_pages_stay() {
    let (_p, pool) = pool(4);
    let ids = make_pages(&pool, 10);
    let held: Vec<_> = ids[..3].iter().map(|id| pool.fetch(*id).unwrap()).collect();
    for _ in 0..5 {
        for id in &ids[3..] {
            assert_eq!(tag(&pool, *id), id.0); // churns the one unpinned frame
        }
    }
    let hits_before = pool.stats().hits;
    for r in &held {
        assert_eq!(read_u64_at(r.read().payload(), 0), r.id().0);
        drop(pool.fetch(r.id()).unwrap());
    }
    assert_eq!(pool.stats().hits, hits_before + 3); // still cached

    let fourth = pool.fetch(ids[5]).unwrap();
    assert!(matches!(pool.fetch(ids[6]), Err(Error::PoolExhausted)));
    assert!(matches!(pool.free(ids[5]), Err(Error::Pinned(_))));
    drop(fourth);
    pool.fetch(ids[6]).unwrap();
}

#[test]
fn durable_pages_must_be_copied() {
    let (_p, pool) = pool(4);
    let ids = make_pages(&pool, 2);
    pool.sync().unwrap();
    let r = pool.fetch(ids[0]).unwrap();
    assert!(matches!(r.write(), Err(Error::Immutable(_))));
    assert!(!pool.is_writable(ids[0]));
    drop(r);
    // The copy-on-write way: a new page, the old one freed.
    let copy = pool.create(USER).unwrap();
    write_u64_at(copy.write().unwrap().payload_mut(), 0, 99);
    drop(copy);
    pool.free(ids[0]).unwrap();
    pool.sync().unwrap();
    pool.pager().check().unwrap();
}

#[test]
fn flush_then_reopen() {
    let p = SimPlatform::new(1);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    let pager = Arc::new(Pager::create(p.fs(), Path::new("/db/data")).unwrap());
    let pool = BufferPool::new(pager, 8);
    let ids = make_pages(&pool, 20);
    pool.sync().unwrap();
    drop(pool);
    let pager = Arc::new(Pager::open(p.fs(), Path::new("/db/data")).unwrap());
    let pool = BufferPool::new(pager, 8);
    for id in &ids {
        assert_eq!(tag(&pool, *id), id.0);
    }
}

/// Threads read and rewrite their own pages through a pool much smaller than
/// the data, so nearly every fetch evicts something.  Every read must see the
/// thread's last write.
fn stress(platform: &dyn Platform, path: &Path) {
    stress_with(platform, path, 16, 40, 4000);
}

/// [`stress`] over a pool of `frames` (split into shards when large), each
/// thread with `per_thread` pages.
fn stress_with(
    platform: &dyn Platform,
    path: &Path,
    frames: usize,
    per_thread: usize,
    evictions: u64,
) {
    let pager = Arc::new(Pager::create(platform.fs(), path).unwrap());
    let pool = Arc::new(BufferPool::new(pager, frames));
    let threads = 8;
    // More pages per thread than frames, so even a thread running alone
    // evicts on most fetches, whatever the scheduler does.
    let mut pages: Vec<Vec<PageId>> = Vec::new();
    for _ in 0..threads {
        pages.push(make_pages(&pool, per_thread));
    }
    std::thread::scope(|s| {
        for (t, mine) in pages.iter().enumerate() {
            let pool = Arc::clone(&pool);
            s.spawn(move || {
                let rng = SplitMix64::new(t as u64);
                let mut model: BTreeMap<PageId, u64> = mine.iter().map(|id| (*id, id.0)).collect();
                for step in 0..2000u64 {
                    let id = mine[rng.below(mine.len() as u64) as usize];
                    let r = loop {
                        match pool.fetch(id) {
                            Ok(r) => break r,
                            Err(Error::PoolExhausted) => std::thread::yield_now(),
                            Err(e) => panic!("{e}"),
                        }
                    };
                    if rng.below(3) == 0 {
                        let v = (t as u64) << 32 | step;
                        write_u64_at(r.write().unwrap().payload_mut(), 0, v);
                        model.insert(id, v);
                    } else {
                        assert_eq!(read_u64_at(r.read().payload(), 0), model[&id]);
                    }
                }
            });
        }
    });
    assert!(pool.stats().evictions > evictions, "{:?}", pool.stats());
}

#[test]
fn concurrent_stress_sharded() {
    // 1024 frames: four shards, each its own clock; 8 × 1500 pages.
    let p = SimPlatform::new(2);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    stress_with(&p, Path::new("/db/data"), 1024, 1500, 4000);
}

#[test]
fn concurrent_stress_simulated() {
    let p = SimPlatform::new(1);
    p.fs().create_dir_all(Path::new("/db")).unwrap();
    stress(&p, Path::new("/db/data"));
}

#[test]
fn concurrent_stress_real() {
    let dir = std::env::temp_dir().join(format!("nucleus-pool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    stress(&RealPlatform::new().unwrap(), &dir.join("data"));
    std::fs::remove_dir_all(&dir).unwrap();
}
