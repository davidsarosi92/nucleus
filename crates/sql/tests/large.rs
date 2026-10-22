//! Large values stored out of line (PLAN.md §M14.26): read back whole,
//! kept when an update leaves them, removed with their row, moved with a
//! changed primary key, and checked by the consistency check.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use nucleus_kv::{Database, Kv, KvIter, KvTxn, Options, Result, Timestamp, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, catalog, large};

/// A store that counts writes of chunks (keys under a table's
/// [`large::LARGE_INDEX`]).
struct Counting {
    inner: Arc<dyn Kv>,
    chunk_writes: Arc<AtomicUsize>,
}

struct CountingTxn {
    inner: Box<dyn KvTxn>,
    chunk_writes: Arc<AtomicUsize>,
}

fn is_chunk(key: &[u8]) -> bool {
    key.len() > 9 && key[0] == 0x01 && key[5..9] == large::LARGE_INDEX.to_be_bytes()
}

impl Kv for Counting {
    fn begin(&self, opts: TxnOptions) -> Result<Box<dyn KvTxn>> {
        Ok(Box::new(CountingTxn {
            inner: self.inner.begin(opts)?,
            chunk_writes: self.chunk_writes.clone(),
        }))
    }
    fn wall_clock(&self) -> SystemTime {
        self.inner.wall_clock()
    }
    fn back_off(&self, attempt: u32) {
        self.inner.back_off(attempt)
    }
    fn random_bytes(&self, buf: &mut [u8]) {
        self.inner.random_bytes(buf)
    }
}

impl KvTxn for CountingTxn {
    fn read_ts(&self) -> Timestamp {
        self.inner.read_ts()
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        self.inner.scan(start, end)
    }
    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get_validated(key)
    }
    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        self.inner.scan_validated(start, end)
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if is_chunk(key) {
            self.chunk_writes.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> Result<()> {
        if is_chunk(key) {
            self.chunk_writes.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.delete(key)
    }
    fn commit(self: Box<Self>) -> Result<Timestamp> {
        self.inner.commit()
    }
    fn rollback(self: Box<Self>) {
        self.inner.rollback()
    }
}

fn setup() -> (Arc<dyn Kv>, Arc<AtomicUsize>) {
    let inner: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(3)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let writes = Arc::new(AtomicUsize::new(0));
    let kv = Arc::new(Counting {
        inner,
        chunk_writes: writes.clone(),
    });
    (kv, writes)
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn chunk_count(kv: &dyn Kv) -> usize {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let t = catalog::table(&*txn, "files").unwrap();
    let p = t.index_prefix(large::LARGE_INDEX);
    let end = nucleus_kv::prefix_end(&p).unwrap();
    txn.scan(Bound::Included(&p[..]), Bound::Excluded(&end[..]))
        .unwrap()
        .count()
}

fn consistent(kv: &dyn Kv) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn large_values_live_out_of_line() {
    let (kv, writes) = setup();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE files (id INT PRIMARY KEY, name TEXT, body BYTEA, note TEXT)")
        .unwrap();
    // 100 000 bytes of body (several chunks) and a 3 000-character note (one).
    s.execute(
        "INSERT INTO files VALUES (1, 'a.bin', decode(repeat('ab', 100000), 'hex'), \
         repeat('n', 3000)), (2, 'small', '\\x01', 'short')",
    )
    .unwrap();
    let body_chunks = 100_005usize.div_ceil(large::CHUNK);
    assert_eq!(chunk_count(&*kv), body_chunks + 1);
    assert_eq!(
        rows(
            &mut s,
            "SELECT id, length(body), md5(body), length(note) FROM files ORDER BY id"
        ),
        [
            ["1", "100000", "e5c1ab047ce869dab2253aa526431e59", "3000"],
            ["2", "1", "55a54008ad1ba589aa210d2629c1df41", "5"],
        ]
        .map(|r| r.map(String::from).to_vec())
    );
    consistent(&*kv);

    // An update that leaves the large values writes no chunks.
    writes.store(0, Ordering::Relaxed);
    s.execute("UPDATE files SET name = 'b.bin' WHERE id = 1")
        .unwrap();
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    assert_eq!(
        rows(
            &mut s,
            "SELECT length(body) FROM files WHERE name = 'b.bin'"
        ),
        [["100000"]]
    );

    // Changing one rewrites only its chunks; a small value drops them.
    s.execute("UPDATE files SET note = repeat('m', 2500) WHERE id = 1")
        .unwrap();
    assert_eq!(chunk_count(&*kv), body_chunks + 1);
    s.execute("UPDATE files SET note = 'tiny' WHERE id = 1")
        .unwrap();
    assert_eq!(chunk_count(&*kv), body_chunks);
    consistent(&*kv);

    // A changed primary key moves them; a deleted row takes them along.
    s.execute("UPDATE files SET id = 10 WHERE id = 1").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT md5(body) FROM files WHERE id = 10"),
        [["e5c1ab047ce869dab2253aa526431e59"]]
    );
    assert_eq!(chunk_count(&*kv), body_chunks);
    consistent(&*kv);
    s.execute("DELETE FROM files WHERE id = 10").unwrap();
    assert_eq!(chunk_count(&*kv), 0);
    consistent(&*kv);

    // Through rewrites of the whole table: a column added with a default,
    // a column's type changed.
    s.execute("INSERT INTO files VALUES (3, 'c', decode(repeat('cd', 5000), 'hex'), NULL)")
        .unwrap();
    s.execute("ALTER TABLE files ADD COLUMN flag BOOLEAN DEFAULT true")
        .unwrap();
    s.execute("ALTER TABLE files ALTER COLUMN id TYPE BIGINT")
        .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT id, length(body), flag FROM files ORDER BY id"
        ),
        [["2", "1", "t"], ["3", "5000", "t"]].map(|r| r.map(String::from).to_vec())
    );
    consistent(&*kv);

    // A rolled-back write leaves nothing behind.
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO files VALUES (4, 'd', decode(repeat('ef', 9000), 'hex'), NULL)")
        .unwrap();
    s.execute("ROLLBACK").unwrap();
    assert_eq!(chunk_count(&*kv), 1);
    consistent(&*kv);
}

/// The check finds chunks without their row, and a value missing chunks.
#[test]
fn the_check_finds_lost_and_orphaned_chunks() {
    let (kv, _) = setup();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE files (id INT PRIMARY KEY, body BYTEA)")
        .unwrap();
    s.execute("INSERT INTO files VALUES (1, decode(repeat('ab', 50000), 'hex'))")
        .unwrap();
    let mut txn = kv.begin(TxnOptions::default()).unwrap();
    let t = catalog::table(&*txn, "files").unwrap();
    let p = t.index_prefix(large::LARGE_INDEX);
    let end = nucleus_kv::prefix_end(&p).unwrap();
    let first: Vec<u8> = txn
        .scan(Bound::Included(&p[..]), Bound::Excluded(&end[..]))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .0;
    txn.delete(&first).unwrap();
    let mut orphan = p.clone();
    orphan.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09]);
    orphan.extend_from_slice(&[0, 1, 0, 0, 0, 0]);
    txn.put(&orphan, b"stray").unwrap();
    txn.commit().unwrap();
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems =
        nucleus_sql::check::check(&*txn, &nucleus_sql::ext::Registry::builtin()).unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("1 values stored out of line lack chunks, 1 have no row")),
        "{problems:#?}"
    );
}
