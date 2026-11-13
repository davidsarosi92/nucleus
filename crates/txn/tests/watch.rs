//! Watched key ranges ([`Engine::watch`]): a commit writing into one
//! moves the mark a snapshot's reader checks what it kept against.

use std::path::Path;
use std::sync::Arc;

use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_storage::StoreOptions;
use nucleus_txn::{Engine, EngineOptions, Isolation, TxnOptions};

fn engine(opts: EngineOptions) -> Engine {
    let p: Arc<dyn Platform> = Arc::new(SimPlatform::new(1));
    Engine::open(p, Path::new("/db"), opts).unwrap()
}

fn put(e: &Engine, k: &[u8]) {
    let mut t = e.begin();
    t.put(k, b"v").unwrap();
    t.commit().unwrap();
}

#[test]
fn commits_into_a_watched_range_move_the_mark() {
    let e = engine(EngineOptions::default());
    let before = e.begin().read_ts();
    put(&e, b"a");
    // Watching begins after: nothing read before is known current.
    e.watch(b"c", b"d");
    assert!(!e.begin().watched_as_at(before));
    let at = e.begin().read_ts();
    assert!(e.begin().watched_as_at(at));
    // Writes outside the range leave the mark.
    put(&e, b"b");
    put(&e, b"d");
    assert!(e.begin().watched_as_at(at));
    // A snapshot older than the mark, or one the mark passed, does not.
    let old = e.begin();
    put(&e, b"c1");
    assert!(!e.begin().watched_as_at(at));
    assert!(!old.watched_as_at(at));
    let now = e.begin().read_ts();
    assert!(!old.watched_as_at(now));
    assert!(e.begin().watched_as_at(now));
    // A transaction's own write into the range.
    let mut t = e.begin();
    assert!(t.watched_as_at(now));
    t.put(b"c2", b"v").unwrap();
    assert!(!t.watched_as_at(now));
    t.rollback();
    assert!(e.begin().watched_as_at(now));
    // A serializable transaction that writes; one that only reads.
    let ser = |read_only| {
        e.begin_with(TxnOptions {
            isolation: Isolation::Serializable,
            read_only,
            ..TxnOptions::default()
        })
        .unwrap()
    };
    assert!(!ser(false).watched_as_at(now));
    assert!(ser(true).watched_as_at(now));
}

#[test]
fn spilled_writes_into_a_watched_range_count() {
    let e = engine(EngineOptions {
        store: StoreOptions {
            cache_pages: 64,
            wal_segment_size: 1 << 16,
        },
        checkpoint_wal_bytes: 1 << 20,
        history: 0,
        history_time: 0,
        txn_memory: 2048,
        wal_minimal: true,
    });
    e.watch(b"c", b"d");
    let at = e.begin().read_ts();
    let mut t = e.begin();
    t.put(b"c", &[7; 100]).unwrap();
    for i in 0..1000u32 {
        t.put(format!("x{i:06}").as_bytes(), &[7; 100]).unwrap();
    }
    assert!(!t.watched_as_at(at));
    t.commit().unwrap();
    assert!(!e.begin().watched_as_at(at));
}
