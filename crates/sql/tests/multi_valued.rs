//! Multi-valued indexes (PLAN.md §M15 D5) beyond
//! `slt/89_multi_valued_indexes.slt`: on random documents in every
//! storage, each condition an index serves answers as a table without
//! the index does — through inserts, updates and deletes, and an index
//! built online — the indexes are whole (`check`), and the plans read
//! them.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session};

/// The storages a table's rows may have.
const STORAGES: &[&str] = &["", "WITH (storage = column)", "WITH (engine = lsm)"];

fn open(seed: u64) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .into_iter()
            .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// A random document: tags an array (maybe with repeats), a scalar, or
/// absent; items objects whose `sku` is a string or an array of them.
fn document(r: &mut Rng) -> (String, String) {
    let tag = |r: &mut Rng| format!("\"t{}\"", r.below(12));
    let tags = match r.below(6) {
        0 => String::new(),
        1 => format!(", \"tags\": {}", tag(r)),
        _ => {
            let n = r.below(5);
            let t: Vec<String> = (0..n).map(|_| tag(r)).collect();
            format!(", \"tags\": [{}]", t.join(", "))
        }
    };
    let n = r.below(4);
    let items: Vec<String> = (0..n)
        .map(|_| {
            let sku = if r.below(5) == 0 {
                format!("[\"s{}\", \"s{}\"]", r.below(20), r.below(20))
            } else {
                format!("\"s{}\"", r.below(20))
            };
            format!("{{\"sku\": {sku}, \"n\": {}}}", r.below(50))
        })
        .collect();
    let arr: Vec<String> = (0..r.below(4))
        .map(|_| format!("'a{}'", r.below(8)))
        .collect();
    let arr = if r.below(8) == 0 {
        "NULL".to_string()
    } else {
        format!("ARRAY[{}]::text[]", arr.join(", "))
    };
    (
        format!("{{\"k\": 1{tags}, \"items\": [{}]}}", items.join(", ")),
        arr,
    )
}

/// The conditions compared, each with whether its plan reads an index.
fn conditions() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for t in [0, 3, 11, 12] {
        out.push((format!("doc -> 'tags' @> '\"t{t}\"'"), true));
        out.push((format!("doc @> '{{\"tags\": [\"t{t}\"]}}'"), true));
        out.push((format!("doc @> '{{\"tags\": \"t{t}\"}}'"), true));
    }
    out.push(("doc -> 'tags' @> '[\"t1\", \"t2\"]'".into(), true));
    for s in [0, 7, 19] {
        out.push((format!("doc -> 'items' @> '[{{\"sku\": \"s{s}\"}}]'"), true));
        out.push((
            format!("doc @> '{{\"items\": [{{\"sku\": [\"s{s}\"]}}]}}'"),
            true,
        ));
        out.push((
            format!("doc @? 'strict $.items[*].sku ? (@ == \"s{s}\")'"),
            true,
        ));
    }
    for (op, mirror, n) in [
        (">", "<", 45),
        (">=", "<=", 48),
        ("<", ">", 2),
        ("<=", ">=", 0),
        ("==", "==", 30),
    ] {
        out.push((format!("doc @? 'strict $.items[*] ? (@.n {op} {n})'"), true));
        out.push((
            format!("doc @? 'strict $.items[*] ? ({n} {mirror} @.n)'"),
            true,
        ));
        // Lax: no bound, the same answer.
        out.push((format!("doc @? '$.items[*] ? (@.n {op} {n})'"), false));
    }
    for a in [0, 5, 9] {
        out.push((format!("'a{a}' = ANY(arr)"), true));
    }
    // Two bounds met by two elements: one range of the part would miss
    // the row.
    out.push((
        "doc @? 'strict $.items[*] ? (@.n > 45)' AND doc @? 'strict $.items[*] ? (@.n < 3)'".into(),
        true,
    ));
    out
}

fn compare(s: &mut Session, what: &str) {
    for (c, indexed) in conditions() {
        let with = rows(s, &format!("SELECT id FROM p WHERE {c} ORDER BY id"));
        let without = rows(s, &format!("SELECT id FROM q WHERE {c} ORDER BY id"));
        assert_eq!(with, without, "{what}: {c}");
        if indexed {
            let plan = rows(s, &format!("EXPLAIN SELECT id FROM p WHERE {c}")).join("\n");
            assert!(plan.contains("Index Scan using p_"), "{what}: {c}\n{plan}");
        }
    }
}

#[test]
fn indexed_conditions_answer_as_a_scan_in_every_storage() {
    for (n, storage) in STORAGES.iter().enumerate() {
        let kv = open(n as u64 + 1);
        let mut s = Session::new(Arc::clone(&kv));
        let mut r = Rng(42 + n as u64);
        for t in ["p", "q"] {
            s.execute(&format!(
                "CREATE TABLE {t} (id int PRIMARY KEY, doc json, arr text[]) {storage}"
            ))
            .unwrap();
        }
        // The indexes on p before its rows (kept by each write), but one
        // built online after them.
        s.execute(
            "CREATE INDEX p_tags ON p ((json_array_elements(doc -> 'tags'))); \
             CREATE INDEX p_sku ON p ((json_array_elements(doc -> 'items') -> 'sku')); \
             CREATE INDEX p_arr ON p ((unnest(arr)))",
        )
        .unwrap();
        for id in 0..1500 {
            let (doc, arr) = document(&mut r);
            for t in ["p", "q"] {
                s.execute(&format!("INSERT INTO {t} VALUES ({id}, '{doc}', {arr})"))
                    .unwrap();
            }
        }
        s.execute(
            "CREATE INDEX CONCURRENTLY p_n ON p ((json_array_elements(doc -> 'items') -> 'n'), id)",
        )
        .unwrap();
        s.execute("ANALYZE p; ANALYZE q").unwrap();
        check(&kv, &s, storage);
        compare(&mut s, storage);
        // Rewritten, moved and deleted rows.
        for _ in 0..300 {
            let id = r.below(1500);
            let sql = match r.below(4) {
                0 => {
                    let (doc, arr) = document(&mut r);
                    format!("UPDATE {{t}} SET doc = '{doc}', arr = {arr} WHERE id = {id}")
                }
                1 => format!(
                    "UPDATE {{t}} SET doc = json_set(doc, '{{tags}}', '[\"t{}\", \"t{}\"]') WHERE id = {id}",
                    r.below(12),
                    r.below(12)
                ),
                2 => format!("UPDATE {{t}} SET id = id + 10000 WHERE id = {id}"),
                _ => format!("DELETE FROM {{t}} WHERE id = {id}"),
            };
            for t in ["p", "q"] {
                s.execute(&sql.replace("{t}", t)).unwrap();
            }
        }
        check(&kv, &s, storage);
        compare(&mut s, &format!("{storage}, after writes"));
    }
}
