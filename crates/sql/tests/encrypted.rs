//! An encrypted database end to end (PLAN.md §M14.30 K2b–K2d): values
//! written through SQL into every kind of structure — a row table with a
//! text index, a column table, JSON documents, a vector table with an
//! HNSW index — are in some file of a database in the clear and in no
//! file of an encrypted one, checkpointed or only logged; a sort past
//! `work_mem` meanwhile goes through sealed scratch files; the database
//! reopens under its key with everything there.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, PageCipher};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

const NEEDLES: [&str; 4] = [
    "rowtext-planted-7f3a19c2",
    "column-planted-5e81d0b4",
    "jsondoc-planted-c29e6a17",
    "vectag-planted-0b4d8f53",
];

fn open(p: &Arc<SimPlatform>, encrypted: bool) -> Database {
    Database::open_on(
        Arc::clone(p) as Arc<dyn Platform>,
        Path::new("/db"),
        Options {
            encryption_key: encrypted.then(|| PageCipher::new([2; 32])),
            ..Options::default()
        },
    )
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

/// The needles found in any file under `dir`, raw.
fn found(p: &SimPlatform, dir: &Path, out: &mut Vec<String>) {
    let mut names = p.fs().list_dir(dir).unwrap();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        if p.fs().list_dir(&path).is_ok() {
            found(p, &path, out);
            continue;
        }
        let file = p
            .fs()
            .open(&path, nucleus_platform::OpenMode::ReadOnly)
            .unwrap();
        let mut bytes = vec![0u8; file.size().unwrap() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        for n in NEEDLES {
            if bytes.windows(n.len()).any(|w| w == n.as_bytes()) {
                out.push(format!("{n} in {}", path.display()));
            }
        }
    }
}

fn write(s: &mut Session, from: i64, to: i64) {
    s.execute(&format!(
        "INSERT INTO docs SELECT i, '{0} ' || i FROM generate_series({from}, {to}) i; \
         INSERT INTO cols SELECT i, '{1} ' || i FROM generate_series({from}, {to}) i; \
         INSERT INTO docs_json SELECT i, ('{{\"note\": \"{2} ' || i || '\"}}')::json \
           FROM generate_series({from}, {to}) i; \
         INSERT INTO items SELECT i, ARRAY[i, i % 7, 1]::vector(3), '{3} ' || i \
           FROM generate_series({from}, {to}) i",
        NEEDLES[0], NEEDLES[1], NEEDLES[2], NEEDLES[3]
    ))
    .unwrap();
}

#[test]
fn values_written_through_sql_are_in_no_file_of_an_encrypted_database() {
    for encrypted in [false, true] {
        let p = Arc::new(SimPlatform::new(5));
        {
            let db = open(&p, encrypted);
            let kv: Arc<dyn Kv> = Arc::new(db.clone());
            let mut s = Session::new(kv);
            s.execute(
                "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT); \
                 CREATE INDEX docs_body ON docs USING text (body); \
                 CREATE TABLE cols (k INT PRIMARY KEY, v TEXT) WITH (storage = column); \
                 CREATE TABLE docs_json (id INT PRIMARY KEY, doc JSON); \
                 CREATE TABLE items (id INT PRIMARY KEY, v vector(3), tag TEXT); \
                 CREATE INDEX items_v ON items USING hnsw (v)",
            )
            .unwrap();
            // Checkpointed into the data file, then more only in the log.
            write(&mut s, 1, 3000);
            db.checkpoint().unwrap();
            write(&mut s, 3001, 3200);
            // A sort past `work_mem`: through scratch files.
            let before = db.scratch_files_made();
            s.execute("SET work_mem = '64kB'").unwrap();
            let sorted = rows(&mut s, "SELECT body FROM docs ORDER BY body DESC");
            assert_eq!(sorted.len(), 3200);
            assert!(db.scratch_files_made() > before, "the sort spilled");
        }
        let mut hits = Vec::new();
        found(&p, Path::new("/db"), &mut hits);
        if encrypted {
            assert!(hits.is_empty(), "{hits:#?}");
        } else {
            for n in NEEDLES {
                assert!(hits.iter().any(|h| h.starts_with(n)), "{n}: {hits:#?}");
            }
        }

        let db = open(&p, encrypted);
        let kv: Arc<dyn Kv> = Arc::new(db.clone());
        let mut s = Session::new(kv);
        let counts = rows(
            &mut s,
            "SELECT (SELECT count(*) FROM docs), (SELECT count(*) FROM cols), \
               (SELECT count(*) FROM docs_json), (SELECT count(*) FROM items)",
        );
        assert_eq!(
            counts,
            vec![vec![Value::Int(3200); 4]],
            "encrypted: {encrypted}"
        );
        let near = rows(
            &mut s,
            "SELECT id FROM items ORDER BY v <-> '[3100, 6, 1]' LIMIT 1",
        );
        assert_eq!(near, vec![vec![Value::Int(3100)]]);
        let matched = rows(
            &mut s,
            &format!("SELECT count(*) FROM docs WHERE body @@ '{}'", NEEDLES[0]),
        );
        assert_ne!(matched, vec![vec![Value::Int(0)]], "the text index answers");
    }
}
