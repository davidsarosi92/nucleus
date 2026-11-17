//! What the LSM tree writes (PLAN.md §M14.28 L2): the study's
//! steady-state workload — rows of 108 bytes (an 8-byte key, 100 bytes of
//! value) under random keys into a table much larger than the cache,
//! 100 rows a commit — on the B+tree and on a route to the LSM tree, the
//! bytes each writes to its files (WAL, data file, engines' files) per
//! byte of rows, and the time.  Real files, a counting file system around
//! them; a checkpoint every 16 MiB of rows (the store's default is a GiB
//! of WAL; scaled with the table).
//!
//! `cargo test --release -p nucleus-storage --test lsm_wamp -- --ignored --nocapture`
//!
//! Each line is printed as an `@metric` line too, for `bench/method.py
//! run` (PLAN.md §M14.31).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use nucleus_platform::real::RealPlatform;
use nucleus_platform::{Clock, File, FileSystem, Lock, OpenMode, Platform, Rng, SplitMix64};
use nucleus_storage::{Batch, EngineKind, Store, StoreOptions};

/// Bytes written: the WAL, the data file, the engines' files.
#[derive(Default)]
struct Counts {
    wal: AtomicU64,
    data: AtomicU64,
    engines: AtomicU64,
}

impl Counts {
    fn of(&self, path: &Path) -> &AtomicU64 {
        let p = path.to_string_lossy();
        if p.contains("/wal") {
            &self.wal
        } else if p.contains("/engines") {
            &self.engines
        } else {
            &self.data
        }
    }

    fn get(&self) -> (u64, u64, u64) {
        (
            self.wal.load(Ordering::Relaxed),
            self.data.load(Ordering::Relaxed),
            self.engines.load(Ordering::Relaxed),
        )
    }
}

/// The real file system, its writes counted.
struct CountingFs {
    inner: RealPlatform,
    counts: Arc<Counts>,
}

struct CountingFile {
    inner: Box<dyn File>,
    counts: Arc<Counts>,
    path: PathBuf,
}

impl File for CountingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let n = self.inner.write_at(buf, offset)?;
        self.counts
            .of(&self.path)
            .fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
    fn sync_order(&self) -> io::Result<()> {
        self.inner.sync_order()
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

impl FileSystem for CountingFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        Ok(Box::new(CountingFile {
            inner: self.inner.fs().open(path, mode)?,
            counts: Arc::clone(&self.counts),
            path: path.to_path_buf(),
        }))
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.fs().remove_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.fs().rename(from, to)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.fs().create_dir_all(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.fs().remove_dir(path)
    }
    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        self.inner.fs().list_dir(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.fs().exists(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.fs().sync_dir(path)
    }
    fn writes_in_place(&self) -> bool {
        self.inner.fs().writes_in_place()
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn Lock>> {
        self.inner.fs().lock(path)
    }
}

/// The real platform with its writes counted.
struct Plat(CountingFs);

impl Platform for Plat {
    fn fs(&self) -> &dyn FileSystem {
        &self.0
    }
    fn clock(&self) -> &dyn Clock {
        self.0.inner.clock()
    }
    fn rng(&self) -> &dyn Rng {
        self.0.inner.rng()
    }
    fn parallelism(&self) -> usize {
        self.0.inner.parallelism()
    }
}

const ROWS: u64 = 1_000_000;
const MORE: u64 = 1_000_000;
const VALUE: usize = 100;
const CHECKPOINT: u64 = 16 << 20;

fn row(rng: &SplitMix64) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut k = b"t".to_vec();
    k.extend_from_slice(&rng.next_u64().to_be_bytes());
    // Random bytes: a value no block compression shrinks, so the LSM
    // tree's compressed blocks get no edge the B+tree's pages lack.
    let v: Vec<u8> = (0..VALUE.div_ceil(8))
        .flat_map(|_| rng.next_u64().to_le_bytes())
        .take(VALUE)
        .collect();
    (k, Some(v))
}

fn run(kind: Option<EngineKind>) {
    let name = kind.map_or("B+tree".to_owned(), |k| format!("{k:?}"));
    let dir = std::env::temp_dir().join(format!("nucleus-lsm-wamp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let p = Arc::new(Plat(CountingFs {
        inner: RealPlatform::new().unwrap(),
        counts: Arc::new(Counts::default()),
    }));
    let opts = StoreOptions {
        cache_pages: 2048, // 16 MiB: the table is ~8 times larger
        ..StoreOptions::default()
    };
    let (store, _) = Store::open(Arc::clone(&p) as Arc<dyn Platform>, &dir, &opts).unwrap();
    if let Some(k) = kind {
        store.add_engine(b"t", b"u", k).unwrap();
    }
    let rng = SplitMix64::new(7);
    let mut sample = Vec::new();
    // The table: 1M rows, 10,000 a commit, checkpointed.
    let mut meta = 0;
    for _ in 0..ROWS / 10_000 {
        let mut ops: Vec<_> = (0..10_000).map(|_| row(&rng)).collect();
        sample.extend(ops.iter().step_by(100).map(|o| o.0.clone()));
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        ops.dedup_by(|a, b| a.0 == b.0);
        meta += 1;
        store.commit(&Batch { meta, ops }).unwrap();
    }
    store.checkpoint().unwrap();
    let before = p.0.counts.get();
    // The steady state: 1M rows under random keys, 100 a commit.
    let t = Instant::now();
    let mut since = 0u64;
    for _ in 0..MORE / 100 {
        let mut ops: Vec<_> = (0..100).map(|_| row(&rng)).collect();
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        ops.dedup_by(|a, b| a.0 == b.0);
        meta += 1;
        store.commit(&Batch { meta, ops }).unwrap();
        since += 100 * (9 + VALUE as u64);
        if since >= CHECKPOINT {
            store.checkpoint().unwrap();
            since = 0;
        }
    }
    store.checkpoint().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let after = p.0.counts.get();
    // Point lookups of rows the table had, after the writes.
    let t = Instant::now();
    for k in &sample {
        assert!(store.get(k).unwrap().is_some());
    }
    let get_us = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
    let rows = (MORE * (9 + VALUE as u64)) as f64;
    let (w, d, e) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
    let total = (w + d + e) as f64;
    println!(
        "{name}: {MORE} rows, 100 a commit, into {ROWS}: {secs:.1} s; written {:.1} MiB \
         (WAL {:.1}, data file {:.1}, engines {:.1}) = {:.1} bytes a byte of rows; \
         a lookup {get_us:.1} µs",
        total / 1048576.0,
        w as f64 / 1048576.0,
        d as f64 / 1048576.0,
        e as f64 / 1048576.0,
        total / rows
    );
    let metric = |what: &str, v: f64, unit: &str| {
        println!(
            "@metric {{\"name\": \"{what}\", \"values\": [{v}], \"unit\": \"{unit}\", \"system\": \"nucleus\", \"phase\": \"write\"}}"
        );
    };
    metric(&format!("{name}: write amplification"), total / rows, "x");
    metric(&format!("{name}: 1M random rows, 100 a commit"), secs, "s");
    metric(&format!("{name}: a point lookup after"), get_us, "µs");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn what_the_lsm_tree_writes() {
    run(None);
    run(Some(EngineKind::Lsm));
}
