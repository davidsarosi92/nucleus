//! The erasure's measurement (PLAN.md §M14.24 R2c; docs/benchmarks.md
//! "Erasure"), on real files — not run by default:
//!
//! ```sh
//! ERASURE_DIR=target/erasure-bench cargo test --release -p nucleus-sql \
//!     --test erasure_bench -- --ignored --nocapture
//! ```
//!
//! Without `ERASURE_DIR` (the CI's run of ignored tests) it runs as a
//! test: 10 000 rows in a temporary directory, removed after.
//!
//! A table of `ROWS` (100 000) rows — a subject column, a text with a
//! text index, a vector with an HNSW index — of 1 000 subjects.  A
//! subject's texts are planted and searched for in every file: before
//! and after a plain `DELETE` (and the indexes' upkeep and a vacuum after
//! it), then `FORGET SUBJECT` of other subjects, timed against a `DELETE`
//! of as many rows.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use nucleus_kv::{Database, Kv, Options};
use nucleus_sql::{Output, Session, Value};

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn size(p: &Path) -> u64 {
    std::fs::read_dir(p).map_or(0, |r| {
        r.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => size(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    })
}

#[test]
#[ignore]
fn erasure() {
    let given = std::env::var("ERASURE_DIR").ok();
    let dir = match &given {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::temp_dir().join(format!("nucleus-erasure-{}", std::process::id())),
    };
    let dir = dir.as_path();
    assert!(
        !dir.exists(),
        "{} exists: name a new directory",
        dir.display()
    );
    let kv: Arc<dyn Kv> = Arc::new(Database::open(dir, Options::default()).unwrap());
    let mut s = Session::new(kv.clone());
    let n: usize = std::env::var("ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if given.is_some() { 100_000 } else { 10_000 });
    s.execute(
        "CREATE TABLE m (id int PRIMARY KEY, who text, body text, v vector(4)); \
         ALTER TABLE m SET SUBJECT (who); \
         CREATE INDEX ON m USING text (body); \
         CREATE INDEX ON m USING hnsw (v)",
    )
    .unwrap();
    let t = Instant::now();
    s.execute(&format!(
        "INSERT INTO m SELECT g, 'user-' || (g % 1000), \
           'message ' || g || ' from user-' || (g % 1000) || ' about ' || md5(g::text), \
           ('[' || (g % 97) || ',' || (g % 89) || ',' || (g % 83) || ',' || (g % 79) || ']')::vector \
         FROM generate_series(1, {n}) g"
    ))
    .unwrap();
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    println!(
        "{n} rows loaded in {:?}; {} MiB",
        t.elapsed(),
        size(dir) >> 20
    );
    // The planted texts: a subject's.
    let planted: Vec<Vec<u8>> = rows(&mut s, "SELECT body FROM m WHERE who = 'user-900'")
        .into_iter()
        .map(|r| r[0].render().into_bytes())
        .collect();
    let found = |kv: &Arc<dyn Kv>| kv.find_bytes(&planted).unwrap().unwrap();
    println!(
        "{} texts planted, found in {} places",
        planted.len(),
        found(&kv)
    );
    s.execute("DELETE FROM m WHERE who = 'user-900'").unwrap();
    let planted_found = found(&kv);
    println!("after DELETE: {planted_found}");
    assert!(planted_found >= planted.len() as u64);
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    kv.vacuum().unwrap();
    let t = Instant::now();
    let after = found(&kv);
    println!(
        "after the upkeep and a vacuum: {after} (the search: {:?})",
        t.elapsed()
    );
    for round in 0..3 {
        let t = Instant::now();
        s.execute(&format!("DELETE FROM m WHERE who = 'user-{}'", 500 + round))
            .unwrap();
        let delete = t.elapsed();
        let t = Instant::now();
        let r = rows(&mut s, &format!("FORGET SUBJECT 'user-{}'", 100 + round));
        let forget = t.elapsed();
        let r: Vec<String> = r[0].iter().map(Value::render).collect();
        println!(
            "DELETE {delete:?}, FORGET {forget:?} (erasure {}, rows {}, scrubbed {}, found {})",
            r[0], r[1], r[2], r[3]
        );
        assert_eq!(r[2..], ["t", "0"]);
    }
    s.execute("FORGET SUBJECT 'user-900'").unwrap();
    let left = found(&kv);
    println!("user-900 forgotten too: {left}; {} MiB", size(dir) >> 20);
    assert_eq!(left, 0);
    drop(s);
    drop(kv);
    if given.is_none() {
        let _ = std::fs::remove_dir_all(dir);
    }
}
