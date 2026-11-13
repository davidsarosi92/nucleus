//! What the engine reads back from disk: a database's files — its pages
//! (the B-tree's nodes and overflow, the free list, the headers) and its
//! WAL — damaged by the fuzzer, then opened, checked and read.  A damaged
//! file must fail with an error, never a panic, a hang or a crash.
//!
//! The image is a real database (tables of both storages, keys, indexes
//! of every kind, roles, a model), part of it checkpointed
//! into pages and part only in the WAL.  The fuzzer's input is a list of
//! patches: `[file] [offset: u32] [length] [bytes]`.  A checksum would
//! stop almost every patch at the first read, so by default the patched
//! pages and WAL records are sealed again with their checksums — the
//! damage a checksum cannot see (a bug that wrote a wrong page, a
//! misdirected write, a stale WAL record) — and the input's first byte
//! can leave them unsealed, to check the checksums' own path.
#![no_main]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{FileSystem, OpenMode, Platform};
use nucleus_sql::Session;
use nucleus_sql::ext::Registry;
use nucleus_storage::PAGE_SIZE;
use nucleus_storage::crc32c::crc32c;

const DIR: &str = "/db";
const RECORD_HEADER: usize = 16;

/// Small WAL segments: each input writes the image's files again.
fn options() -> Options {
    Options {
        wal_segment_bytes: 256 << 10,
        ..Options::default()
    }
}

/// The image's files: path and bytes.
fn image() -> &'static Vec<(PathBuf, Vec<u8>)> {
    static I: OnceLock<Vec<(PathBuf, Vec<u8>)>> = OnceLock::new();
    I.get_or_init(|| {
        let p = Arc::new(SimPlatform::new(1));
        let db =
            Arc::new(Database::open_on(p.clone(), Path::new(DIR), options()).expect("a database"));
        let kv: Arc<dyn Kv> = db.clone();
        let mut s = Session::new(kv.clone());
        s.execute(
            "CREATE TABLE docs (id int PRIMARY KEY, title text UNIQUE, body text, \
               n numeric(10, 2), at timestamptz, tags text[], v vector(3)); \
             CREATE INDEX docs_n ON docs (n); \
             CREATE INDEX docs_body ON docs USING text (body); \
             CREATE INDEX docs_v ON docs USING hnsw (v vector_cosine_ops); \
             CREATE TABLE events (id int PRIMARY KEY, kind text, x double precision) \
               WITH (storage = column); \
             CREATE ROLE reader; CREATE USER alice PASSWORD 'secret-secret'; \
             GRANT SELECT ON docs TO reader; \
             CREATE MODEL h USING hashing WITH (dimensions = 4); \
             INSERT INTO docs SELECT i, 'title ' || i, \
               repeat('the quick brown fox jumps over the lazy dog ', i % 7), \
               i * 1.25, '2026-01-01'::timestamptz + i * interval '1 hour', \
               ARRAY['a', 'b' || i], ARRAY[i, i % 5, 1]::vector(3) \
             FROM generate_series(1, 120) i; \
             INSERT INTO docs VALUES (1000, 'long', repeat('x', 20000), 0, NULL, NULL, NULL); \
             INSERT INTO events SELECT i, 'k' || (i % 4), i / 3.0 FROM generate_series(1, 600) i",
        )
        .expect("the image's first part");
        db.checkpoint().expect("a checkpoint");
        // After the checkpoint: only in the WAL.
        s.execute(
            "UPDATE docs SET body = 'changed after the checkpoint' WHERE id % 10 = 0; \
             DELETE FROM docs WHERE id % 17 = 0; \
             INSERT INTO events VALUES (5000, 'late', 1.5); \
             CREATE TABLE later (k text PRIMARY KEY, j int); \
             INSERT INTO later VALUES ('a', 1), ('b', 2)",
        )
        .expect("the image's second part");
        let mut files = Vec::new();
        collect(p.fs(), Path::new(DIR), &mut files);
        drop(s);
        drop(kv);
        drop(db);
        files
    })
}

fn collect(fs: &dyn FileSystem, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for name in fs.list_dir(dir).expect("a directory") {
        let path = dir.join(name);
        match fs.open(&path, OpenMode::ReadOnly) {
            Ok(f) => {
                let mut b = vec![0; f.size().expect("a size") as usize];
                f.read_exact_at(&mut b, 0).expect("the file");
                out.push((path, b));
            }
            // A directory.
            Err(_) => collect(fs, &path, out),
        }
    }
}

fn is_wal(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "wal")
}

/// Seals the pages in `touched` again: each page's checksum of its bytes.
fn seal_pages(b: &mut [u8], touched: &[(usize, usize)]) {
    for &(at, len) in touched {
        for page in at / PAGE_SIZE..=(at + len.max(1) - 1) / PAGE_SIZE {
            let Some(p) = b.get_mut(page * PAGE_SIZE..(page + 1) * PAGE_SIZE) else {
                continue;
            };
            let crc = crc32c(&p[4..]);
            p[..4].copy_from_slice(&crc.to_le_bytes());
        }
    }
}

/// Seals a WAL segment's records again, as far as their lengths lead.
fn seal_wal(path: &Path, b: &mut [u8]) {
    let Some(start) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    let mut at = 0;
    while at + RECORD_HEADER <= b.len() {
        let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let end = at + RECORD_HEADER + len;
        if len == 0 || end > b.len() {
            return;
        }
        let lsn = start + at as u64;
        let mut buf = lsn.to_le_bytes().to_vec();
        buf.extend_from_slice(&b[at + RECORD_HEADER..end]);
        b[at + 4..at + 8].copy_from_slice(&crc32c(&buf).to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&lsn.to_le_bytes());
        at = end;
    }
}

fn read_all(db: Arc<Database>) {
    let kv: Arc<dyn Kv> = db.clone();
    let reg = Registry::builtin();
    if let Ok(txn) = kv.begin(TxnOptions::read_only()) {
        let _ = nucleus_sql::check::check(&*txn, &reg);
    }
    let mut s = Session::new(kv.clone());
    for sql in [
        "SELECT * FROM docs ORDER BY id",
        "SELECT count(*), sum(n), max(title) FROM docs",
        "SELECT id FROM docs WHERE n BETWEEN 10 AND 20",
        "SELECT id FROM docs WHERE title = 'title 7'",
        "SELECT id, score(body @@ 'fox') FROM docs WHERE body @@ 'fox' ORDER BY 2 DESC LIMIT 5",
        "SELECT id FROM docs ORDER BY v <=> '[1, 2, 3]' LIMIT 5",
        "SELECT kind, count(*), sum(x) FROM events GROUP BY kind",
        "SELECT * FROM later",
        "SELECT * FROM nc_catalog.tables",
        "SELECT * FROM nc_catalog.indexes",
        "SELECT * FROM nc_catalog.roles",
        "SELECT * FROM nc_catalog.models",
        "INSERT INTO docs (id, title) VALUES (5001, 'new')",
        "UPDATE docs SET n = n + 1 WHERE id < 50",
        "DELETE FROM events WHERE id < 100",
    ] {
        let _ = s.execute(sql);
    }
    drop(s);
    let _ = db.checkpoint();
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, mut rest)) = data.split_first() else {
        return;
    };
    let mut files = image().clone();
    let mut touched = vec![Vec::new(); files.len()];
    // The patches.
    while rest.len() >= 6 {
        let file = rest[0] as usize % files.len();
        let offset = u32::from_le_bytes(rest[1..5].try_into().unwrap()) as usize;
        let len = (rest[5] as usize).min(rest.len() - 6);
        let bytes = &rest[6..6 + len];
        rest = &rest[6 + len..];
        let b = &mut files[file].1;
        if b.is_empty() {
            continue;
        }
        let at = offset % b.len();
        let end = (at + bytes.len()).min(b.len());
        b[at..end].copy_from_slice(&bytes[..end - at]);
        touched[file].push((at, end - at));
    }
    if mode & 1 == 0 {
        for ((path, b), t) in files.iter_mut().zip(&touched) {
            if t.is_empty() {
                continue;
            }
            if is_wal(path) {
                seal_wal(path, b);
            } else if b.len() % PAGE_SIZE == 0 {
                seal_pages(b, t);
            }
        }
    }
    let p = Arc::new(SimPlatform::new(2));
    for (path, b) in &files {
        p.fs()
            .create_dir_all(path.parent().expect("in the directory"))
            .expect("a directory");
        let f = p.fs().open(path, OpenMode::Create).expect("a file");
        f.write_all_at(b, 0).expect("written");
    }
    if let Ok(db) = Database::open_on(p, Path::new(DIR), options()) {
        read_all(Arc::new(db));
    }
});
