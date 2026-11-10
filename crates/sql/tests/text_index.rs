//! The text index (PLAN.md §M14.21 T4, T5): built over a table's rows,
//! kept on inserts, updates and deletes (a delta, sealed into segments,
//! merged), its answers to `body @@ q` held at every step to the same query
//! evaluated on the rows, its first rows by BM25 to the rows' scores, and
//! its statistics — documents, their length, each term's documents — to
//! the rows' (the integrity check), in every snapshot.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn db(seed: u64) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    fn text(&mut self) -> String {
        const WORDS: [&str; 12] = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
            "gammas", "Delta's",
        ];
        let n = 2 + self.below(10);
        (0..n)
            .map(|_| WORDS[self.below(WORDS.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn ids(s: &mut Session, sql: &str) -> Vec<i64> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r[0].render().parse().unwrap())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn text(s: &mut Session, sql: &str) -> String {
    match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r[0].render())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("{other:?}"),
    }
}

fn problems(kv: &Arc<dyn Kv>, s: &Session) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

const QUERIES: [&str; 11] = [
    "alpha",
    "alpha beta",
    "alpha AND beta",
    "\"alpha beta\"",
    "gam*",
    "delte~1",
    "-alpha",
    "+beta -gamma",
    "(alpha OR delta) AND NOT epsilon",
    "\"delta's iota\"",
    "nothing",
];

/// Every query, through the index and on the rows, alike.
fn same(s: &mut Session, step: &str) {
    for q in QUERIES {
        let q = q.replace('\'', "''");
        let by_index = ids(
            s,
            &format!("SELECT id FROM docs WHERE body @@ '{q}' ORDER BY id"),
        );
        let by_rows = ids(
            s,
            &format!("SELECT id FROM docs WHERE (body || '') @@ '{q}' ORDER BY id"),
        );
        assert_eq!(by_index, by_rows, "{step}: {q}");
        // The first rows by score: ranked by the index from its postings
        // — with other conditions passing all, half, a tenth and a
        // hundredth of the rows, read best first until 7 pass (§M14.21
        // T5a), their scores the index's (T5c) — and, ordered by an
        // expression of the score the index does not rank by, every row
        // it finds scored on its text: alike, scores, ties and all.
        let first = |extra: &str, by: &str| {
            format!(
                "SELECT id::text || ' ' || score(body @@ '{q}')::text FROM docs \
                 WHERE body @@ '{q}'{extra} ORDER BY score(body @@ '{q}'){by} DESC, id LIMIT 7"
            )
        };
        for extra in [
            "",
            " AND n >= 0",
            " AND id % 2 = 0",
            " AND id % 10 = 3",
            " AND id % 100 = 7",
        ] {
            assert!(
                text(s, &format!("EXPLAIN {}", first(extra, "")))
                    .contains(" first 7 by text_score(body) DESC with values"),
                "{step}: {q}{extra}"
            );
            assert!(
                !text(s, &format!("EXPLAIN {}", first(extra, " + 0"))).contains(" first 7 by"),
                "{step}: {q}{extra}"
            );
            assert_eq!(
                text(s, &first(extra, "")),
                text(s, &first(extra, " + 0")),
                "{step}: {q}{extra}"
            );
        }
        // Scored by another query than the condition's: the index cannot
        // rank, and the scores are computed on the rows the condition
        // finds (§M14.21 T5c).
        let other = |by: &str| {
            format!(
                "SELECT id::text || ' ' || score(body @@ 'alpha OR beta')::text FROM docs \
                 WHERE body @@ '{q}' ORDER BY score(body @@ 'alpha OR beta'){by} DESC, id \
                 LIMIT 7"
            )
        };
        assert!(
            text(s, &format!("EXPLAIN {}", other(""))).contains(" with values"),
            "{step}: {q}"
        );
        assert_eq!(text(s, &other("")), text(s, &other(" + 0")), "{step}: {q}");
    }
}

fn shape(kv: &Arc<dyn Kv>) -> (usize, usize) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = nucleus_sql::catalog::find_index(&*txn, "docs_body")
        .unwrap()
        .unwrap();
    nucleus_sql::text::index::shape(&*txn, &t, i).unwrap()
}

#[test]
fn kept_exact_through_writes_seals_and_merges() {
    let kv = db(1);
    let mut s = Session::new(kv.clone());
    let mut r = Lcg(7);
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, n INT, body TEXT)")
        .unwrap();
    for id in 0..3000 {
        let body = if id % 97 == 0 {
            "NULL".to_string()
        } else {
            format!("'{}'", r.text().replace('\'', "''"))
        };
        s.execute(&format!("INSERT INTO docs VALUES ({id}, 0, {body})"))
            .unwrap();
    }
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    assert_eq!(shape(&kv), (1, 0));
    assert!(
        text(&mut s, "EXPLAIN SELECT id FROM docs WHERE body @@ 'alpha'")
            .contains("Index Scan using docs_body on docs (body @@ 'alpha')"),
    );
    same(&mut s, "built");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());

    // A delta over the segment: updates, deletes, inserts, a rollback, an
    // update of another column (no change to the index).
    for id in (0..3000).step_by(6) {
        let body = r.text().replace('\'', "''");
        s.execute(&format!("UPDATE docs SET body = '{body}' WHERE id = {id}"))
            .unwrap();
    }
    s.execute("DELETE FROM docs WHERE id % 10 = 3").unwrap();
    s.execute("UPDATE docs SET n = 1 WHERE id % 10 = 4")
        .unwrap();
    for id in 3000..3600 {
        let body = r.text().replace('\'', "''");
        s.execute(&format!("INSERT INTO docs VALUES ({id}, 0, '{body}')"))
            .unwrap();
    }
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO docs VALUES (9999, 0, 'alpha beta')")
        .unwrap();
    s.execute("DELETE FROM docs WHERE id < 100").unwrap();
    s.execute("ROLLBACK").unwrap();
    // Changed and changed back: two versions of a row in the delta.
    s.execute("UPDATE docs SET body = 'alpha alpha' WHERE id = 1")
        .unwrap();
    s.execute("UPDATE docs SET body = 'beta' WHERE id = 1")
        .unwrap();
    s.execute("UPDATE docs SET body = 'alpha alpha' WHERE id = 1")
        .unwrap();
    // The upkeep sealed a segment once 1,024 rows waited.
    let (segments, waiting) = shape(&kv);
    assert!(segments > 1 && waiting > 0, "{segments} {waiting}");
    same(&mut s, "with a delta");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());

    // Sealed: the delta a segment, its tombstones applied.
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    assert_eq!(shape(&kv).1, 0);
    same(&mut s, "sealed");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());

    // A snapshot from before writes and a seal still counts its rows.
    let old = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = nucleus_sql::catalog::find_index(&*old, "docs_body")
        .unwrap()
        .unwrap();
    let before = nucleus_sql::text::index::stats(&*old, &t, i, &["alpha", "gamma"]).unwrap();

    // Rounds of writes, each sealed: segments of a tier merged.
    for round in 0..12 {
        for k in 0..40 {
            let id = 4000 + round * 40 + k;
            let body = r.text().replace('\'', "''");
            s.execute(&format!("INSERT INTO docs VALUES ({id}, 0, '{body}')"))
                .unwrap();
        }
        s.execute(&format!("DELETE FROM docs WHERE id % 50 = {round}"))
            .unwrap();
        nucleus_sql::text::index::upkeep_now(&kv).unwrap();
        same(&mut s, &format!("round {round}"));
    }
    let (segments, waiting) = shape(&kv);
    assert_eq!(waiting, 0);
    assert!(segments < 12, "merged: {segments} segments");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    assert_eq!(
        nucleus_sql::text::index::stats(&*old, &t, i, &["alpha", "gamma"]).unwrap(),
        before
    );
    drop(old);

    // All merged into one, the deleted rows left out.
    nucleus_sql::text::index::merge_all(&kv, &t, i).unwrap();
    assert_eq!(shape(&kv), (1, 0));
    same(&mut s, "merged");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());

    // Every row deleted, then the table emptied and filled again.
    s.execute("DELETE FROM docs").unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    assert_eq!(shape(&kv), (0, 0));
    same(&mut s, "emptied");
    s.execute("INSERT INTO docs VALUES (1, 0, 'alpha'), (2, 0, NULL)")
        .unwrap();
    s.execute("DELETE FROM docs").unwrap();
    s.execute("INSERT INTO docs VALUES (1, 0, 'Delta''s iota')")
        .unwrap();
    same(&mut s, "emptied and filled");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

#[test]
fn the_index_decides_the_analyzer() {
    let kv = db(2);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    s.execute("INSERT INTO docs VALUES (1, 'Running runners ran'), (2, 'a run')")
        .unwrap();
    // Without an index: the default analyzer, `simple`.
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM docs WHERE body @@ 'run' ORDER BY id"
        ),
        [2]
    );
    s.execute("CREATE INDEX docs_body ON docs USING text (body) WITH (analyzer = 'english')")
        .unwrap();
    // With one: its analyzer, through the index and on the rows alike.
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM docs WHERE body @@ 'run' ORDER BY id"
        ),
        [1, 2]
    );
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM docs WHERE body @@ 'run' OR id = 0 ORDER BY id"
        ),
        [1, 2]
    );
    assert_eq!(
        text(&mut s, "SELECT body @@ 'runs' FROM docs ORDER BY id"),
        "t\nt"
    );
    // A query that varies by row is evaluated on the rows, by the index's
    // analyzer still.
    assert_eq!(
        ids(
            &mut s,
            "SELECT d.id FROM docs d, (VALUES ('running'), ('ran')) q(q) WHERE d.body @@ q.q ORDER BY 1"
        ),
        [1, 1, 2]
    );
    for bad in [
        "CREATE INDEX x ON docs USING text (body) WITH (analyzer = 'klingon')",
        "CREATE INDEX x ON docs USING text (id)",
        "CREATE INDEX x ON docs USING text (body, id)",
        "CREATE INDEX x ON docs USING text (body) WITH (bm25 = 1)",
        "CREATE INDEX x ON docs USING text (body) WITH (b = 1.5)",
    ] {
        assert!(s.execute(bad).is_err(), "{bad}");
    }
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

#[test]
fn a_deleted_row_in_a_segment_stays_deleted() {
    let kv = db(3);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    s.execute("INSERT INTO docs VALUES (1, 'alpha'), (2, 'alpha beta'), (3, 'beta')")
        .unwrap();
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    let q = "SELECT id FROM docs WHERE body @@ 'alpha' ORDER BY id";
    assert_eq!(ids(&mut s, q), [1, 2]);
    s.execute("DELETE FROM docs WHERE id = 1").unwrap();
    assert_eq!(ids(&mut s, q), [2]);
    s.execute("UPDATE docs SET body = 'gamma' WHERE id = 2")
        .unwrap();
    assert_eq!((ids(&mut s, q), shape(&kv)), (vec![], (1, 3)));
    // Sealed: a deleted-documents object and a new segment, apart.
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    assert_eq!((ids(&mut s, q), shape(&kv)), (vec![], (2, 0)));
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM docs WHERE body @@ 'gamma OR beta' ORDER BY id"
        ),
        [2, 3]
    );
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

/// Under row-level security the index ranks the rows the policy lets
/// through, the policy checked first (§M14.21 T5a): a tenant's first rows,
/// as every row of theirs scored on its text gives them.
#[test]
fn ranked_under_row_level_security() {
    let kv = db(3);
    let mut s = Session::new(kv.clone());
    let mut r = Lcg(11);
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, tenant TEXT, body TEXT)")
        .unwrap();
    for id in 0..2000 {
        let tenant = if id % 10 == 0 { "a" } else { "b" };
        let body = r.text().replace('\'', "''");
        s.execute(&format!(
            "INSERT INTO docs VALUES ({id}, '{tenant}', '{body}')"
        ))
        .unwrap();
    }
    for sql in [
        "CREATE INDEX docs_body ON docs USING text (body)",
        "CREATE USER app",
        "GRANT SELECT ON docs TO app",
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY own ON docs USING (tenant = current_setting('nucleus.tenant'))",
        "SET nucleus.tenant = 'a'",
        "SET ROLE app",
    ] {
        s.execute(sql).unwrap();
    }
    for q in QUERIES {
        let q = q.replace('\'', "''");
        let first = |by: &str| {
            format!(
                "SELECT id::text || ' ' || score(body @@ '{q}')::text FROM docs \
                 WHERE body @@ '{q}' ORDER BY score(body @@ '{q}'){by} DESC, id LIMIT 7"
            )
        };
        let plan = text(&mut s, &format!("EXPLAIN {}", first("")));
        assert!(plan.contains(" first 7 by"), "{q}\n{plan}");
        let ranked = text(&mut s, &first(""));
        assert_eq!(ranked, text(&mut s, &first(" + 0")), "{q}");
        for line in ranked.lines() {
            let id: i64 = line.split(' ').next().unwrap().parse().unwrap();
            assert_eq!(id % 10, 0, "{q}: {line}");
        }
    }
}

/// Ties at every limit: rows of few distinct texts, so most scores are
/// shared, ranked by the index at every `LIMIT` and both orders among
/// ties, with and without another condition — the rows tied with the
/// `k`-th are read wherever the slices cut them (§M14.21 T5a, T5b).
#[test]
fn ties_at_every_limit() {
    let kv = db(5);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    const BODIES: [&str; 4] = ["fox fox fox", "fox fox", "fox", "fox and a brown dog"];
    let mut r = Lcg(17);
    for id in 0..120 {
        let body = BODIES[r.below(BODIES.len() as u64) as usize];
        s.execute(&format!("INSERT INTO docs VALUES ({id}, '{body}')"))
            .unwrap();
    }
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    for extra in ["", " AND id % 3 = 1"] {
        for tie in ["id", "id DESC"] {
            for k in 1..=60 {
                let first = |by: &str| {
                    format!(
                        "SELECT id FROM docs WHERE body @@ 'fox'{extra} \
                         ORDER BY score(body @@ 'fox'){by} DESC, {tie} LIMIT {k}"
                    )
                };
                assert!(
                    text(&mut s, &format!("EXPLAIN {}", first(""))).contains(" by text_score"),
                    "{extra} {tie} {k}"
                );
                assert_eq!(
                    ids(&mut s, &first("")),
                    ids(&mut s, &first(" + 0")),
                    "{extra} {tie} {k}"
                );
            }
        }
    }
}

/// Block-max MaxScore (§M14.21 T5b), over an index built in parallel:
/// over a vocabulary as skewed as a
/// language's (Zipf's), texts of very different lengths, terms in many
/// blocks, deleted rows in segments and in the delta, the first rows the
/// index ranks — leaving out what its bounds rule out — are those every
/// row scored on its text gives: scores, ties and all, at every `k`,
/// with filters passing all, half and a fiftieth of the rows.
#[test]
fn pruned_as_scored_in_full() {
    let kv = db(9);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    let mut r = Lcg(23);
    // Word i drawn with a weight of 1 / (i + 1).
    let weights: Vec<f64> = (0..40).map(|i| 1.0 / f64::from(i + 1)).collect();
    let total: f64 = weights.iter().sum();
    let body = |r: &mut Lcg| {
        let long = r.below(4) == 0;
        let n = 1 + r.below(if long { 80 } else { 12 });
        (0..n)
            .map(|_| {
                let mut x = r.below(1 << 30) as f64 / f64::from(1 << 30) * total;
                let mut w = 0;
                while w < 39 && x >= weights[w] {
                    x -= weights[w];
                    w += 1;
                }
                format!("w{w}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let insert = |s: &mut Session, r: &mut Lcg, ids: std::ops::Range<i32>| {
        for chunk in ids.collect::<Vec<_>>().chunks(500) {
            let values: Vec<String> = chunk
                .iter()
                .map(|id| format!("({id}, '{}')", body(r)))
                .collect();
            s.execute(&format!("INSERT INTO docs VALUES {}", values.join(", ")))
                .unwrap();
        }
    };
    insert(&mut s, &mut r, 0..6000);
    // Built on four workers: segments made apart (a batch each), then
    // merged into one.
    s.execute("SET parallel_workers = 4").unwrap();
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    s.execute("SET parallel_workers = 1").unwrap();
    assert_eq!(shape(&kv), (1, 0));
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    // A second segment: rows sealed by the upkeep.
    insert(&mut s, &mut r, 6000..7200);
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    assert_eq!(shape(&kv), (2, 0));
    let check = |s: &mut Session, step: &str| {
        for q in [
            "w0",
            "w0 w1 w2",
            "w3 w17",
            "w25 w2 w0 w39",
            "w1 w1 w30",
            "w1* w5 w12",
            "w0 w1 w2 w3 w4 w5 w6 w7 w8 w9",
        ] {
            for extra in ["", " AND id % 2 = 0", " AND id % 50 = 7"] {
                for k in [1, 3, 10, 40, 200] {
                    let first = |by: &str| {
                        format!(
                            "SELECT id::text || ' ' || score(body @@ '{q}')::text FROM docs \
                             WHERE body @@ '{q}'{extra} \
                             ORDER BY score(body @@ '{q}'){by} DESC, id LIMIT {k}"
                        )
                    };
                    assert_eq!(
                        text(s, &first("")),
                        text(s, &first(" + 0")),
                        "{step}: {q}{extra} {k}"
                    );
                }
            }
        }
    };
    check(&mut s, "built");
    // Deleted rows marked in the segment, more in the delta.
    s.execute("DELETE FROM docs WHERE id % 7 = 2").unwrap();
    insert(&mut s, &mut r, 8000..8600);
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    // Under the seal's 1,024 rows: a delta stays.
    s.execute("DELETE FROM docs WHERE id % 23 = 5").unwrap();
    s.execute("UPDATE docs SET body = 'w0 w0 w0 w1' WHERE id % 31 = 4")
        .unwrap();
    insert(&mut s, &mut r, 9000..9200);
    let (segments, waiting) = shape(&kv);
    assert!(segments > 1 && waiting > 0, "{segments} {waiting}");
    check(&mut s, "with a delta");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}
