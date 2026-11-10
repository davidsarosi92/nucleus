//! Hybrid search (PLAN.md §M14.21 T7): `ORDER BY rrf(…)` and `convex(…)`
//! against the same fusion computed here from every row's score and
//! distance — with indexes (the text index and HNSW ranking the
//! candidates) and without, with a filter and without.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(3)),
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
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows,
        other => panic!("{other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Null => f64::NAN,
        other => panic!("{other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

const WINDOW: usize = 100;

/// The fused values of every candidate, computed from each row's score,
/// whether the query matches it, and distance: `(id, value)`, best first.
/// Each ranking's list its first `WINDOW` rows (the text's of the rows
/// the query matches); a row ranked, or normalized, in each list it is
/// in, and 0 in a list it is not.
fn reference(all: &[(i64, f64, bool, f64)], convex: Option<f64>) -> Vec<(i64, f64)> {
    type Row = (i64, f64, bool, f64);
    let mut text: Vec<&Row> = all.iter().filter(|r| r.2).collect();
    text.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    text.truncate(WINDOW);
    let mut near: Vec<&Row> = all.iter().collect();
    near.sort_by(|a, b| a.3.total_cmp(&b.3).then(a.0.cmp(&b.0)));
    near.truncate(WINDOW);
    let mut cand: Vec<i64> = text.iter().chain(&near).map(|r| r.0).collect();
    cand.sort_unstable();
    cand.dedup();
    // A list's rows' values, best first, and whether greater is better.
    let lists: [(Vec<f64>, Vec<i64>, bool); 2] = [
        (
            text.iter().map(|r| r.1).collect(),
            text.iter().map(|r| r.0).collect(),
            true,
        ),
        (
            near.iter().map(|r| r.3).collect(),
            near.iter().map(|r| r.0).collect(),
            false,
        ),
    ];
    let mut out: Vec<(i64, f64)> = cand
        .iter()
        .map(|&id| {
            let part = |(vals, ids, greater): &(Vec<f64>, Vec<i64>, bool), w: f64| {
                let Some(at) = ids.iter().position(|&x| x == id) else {
                    return 0.0;
                };
                let x = vals[at];
                match convex {
                    None => {
                        let better = vals
                            .iter()
                            .filter(|&&v| if *greater { v > x } else { v < x })
                            .count();
                        1.0 / (60.0 + (1 + better) as f64)
                    }
                    Some(_) => {
                        let lo = vals.iter().copied().fold(f64::MAX, f64::min);
                        let hi = vals.iter().copied().fold(f64::MIN, f64::max);
                        let n = if hi == lo {
                            1.0
                        } else if *greater {
                            (x - lo) / (hi - lo)
                        } else {
                            (hi - x) / (hi - lo)
                        };
                        w * n
                    }
                }
            };
            let (w1, w2) = convex.map_or((1.0, 1.0), |a| (a, 1.0 - a));
            (id, part(&lists[0], w1) + part(&lists[1], w2))
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[test]
fn fused_as_computed_from_every_row() {
    let mut s = Session::new(db());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, n INT, body TEXT, emb vector(4))")
        .unwrap();
    const WORDS: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];
    let mut r = Lcg(11);
    for id in 0..400 {
        let len = 2 + r.below(8);
        let body: Vec<&str> = (0..len)
            .map(|_| WORDS[r.below(WORDS.len() as u64) as usize])
            .collect();
        let v: Vec<String> = (0..4)
            .map(|_| format!("{}", r.below(1000) as f64 / 100.0))
            .collect();
        s.execute(&format!(
            "INSERT INTO docs VALUES ({id}, {}, '{}', '[{}]')",
            r.below(100),
            body.join(" "),
            v.join(",")
        ))
        .unwrap();
    }
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    let q = "alpha beta";
    let v = "[1,2,3,4]";
    for filter in ["", " WHERE n < 50"] {
        // Every row: its score, whether the query matches it, its distance.
        let all: Vec<(i64, f64, bool, f64)> = rows(
            &mut s,
            &format!(
                "SELECT id, score(body @@ '{q}'), body @@ '{q}', emb <=> '{v}' FROM docs{filter}"
            ),
        )
        .iter()
        .map(|r| {
            (
                int(&r[0]),
                float(&r[1]),
                matches!(r[2], Value::Bool(true)),
                float(&r[3]),
            )
        })
        .collect();
        for (fusion, convex) in [
            (format!("rrf(score(body @@ '{q}'), emb <=> '{v}')"), None),
            (
                format!("convex(score(body @@ '{q}'), emb <=> '{v}', 0.3)"),
                Some(0.3),
            ),
        ] {
            let want = reference(&all, convex);
            for k in [1, 10, 50] {
                let sql = format!(
                    "SELECT id, {fusion} FROM docs{filter} ORDER BY {fusion} DESC, id LIMIT {k}"
                );
                let got: Vec<(i64, f64)> = rows(&mut s, &sql)
                    .iter()
                    .map(|r| (int(&r[0]), float(&r[1])))
                    .collect();
                assert_eq!(got.len(), k.min(want.len()), "{sql}");
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.0, w.0, "{sql}: {got:?}");
                    assert!((g.1 - w.1).abs() < 1e-9, "{sql}: {g:?} {w:?}");
                }
            }
        }
        // With HNSW too: on 400 rows its search is exact.
        if filter.is_empty() {
            s.execute(
                "CREATE INDEX docs_emb ON docs USING hnsw (emb vector_cosine_ops) \
                 WITH (m = 16, ef_construction = 400)",
            )
            .unwrap();
        }
    }
    // Other columns read above: each candidate's own.
    let fusion = format!("rrf(score(body @@ '{q}'), emb <=> '{v}')");
    let ids = rows(
        &mut s,
        &format!("SELECT id FROM docs ORDER BY {fusion} DESC, id LIMIT 10"),
    );
    let with = rows(
        &mut s,
        &format!("SELECT upper(body), n + id, id FROM docs ORDER BY {fusion} DESC, id LIMIT 10"),
    );
    for (a, b) in ids.iter().zip(&with) {
        assert_eq!(a[0], b[2]);
        let own = rows(
            &mut s,
            &format!(
                "SELECT upper(body), n + id FROM docs WHERE id = {}",
                int(&a[0])
            ),
        );
        assert_eq!(own[0][..], b[..2]);
    }
    let plan = rows(
        &mut s,
        &format!(
            "EXPLAIN SELECT id FROM docs ORDER BY rrf(score(body @@ '{q}'), emb <=> '{v}') DESC LIMIT 10"
        ),
    );
    let plan: Vec<String> = plan.iter().map(|r| r[0].render()).collect();
    let plan = plan.join("\n");
    // Each ranking its own list, the text index's first 100 by BM25 with
    // their scores (not computed again).
    assert!(
        plan.contains("first 100 by text_score(body) DESC with values"),
        "{plan}"
    );
    assert!(plan.contains("Union All"), "{plan}");
}

/// Where HNSW costs less than a scan (a hundred of 12,000 rows), it
/// gives the nearest candidates.
#[test]
fn nearest_candidates_from_hnsw() {
    let mut s = Session::new(db());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT, emb vector(4))")
        .unwrap();
    let mut r = Lcg(5);
    let mut values = Vec::new();
    for id in 0..12_000 {
        let v: Vec<String> = (0..4).map(|_| format!("{}", r.below(1000))).collect();
        values.push(format!("({id}, 'w{}', '[{}]')", r.below(50), v.join(",")));
    }
    s.execute(&format!("INSERT INTO docs VALUES {}", values.join(",")))
        .unwrap();
    s.execute("CREATE INDEX docs_body ON docs USING text (body)")
        .unwrap();
    s.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb vector_cosine_ops)")
        .unwrap();
    s.execute("ANALYZE docs").unwrap();
    let plan = rows(
        &mut s,
        "EXPLAIN SELECT id FROM docs ORDER BY rrf(score(body @@ 'w7'), emb <=> '[1,2,3,4]') DESC LIMIT 10",
    );
    let plan: Vec<String> = plan.iter().map(|r| r[0].render()).collect();
    let plan = plan.join("\n");
    assert!(
        plan.contains("first 100 by text_score(body) DESC"),
        "{plan}"
    );
    assert!(plan.contains("100 nearest emb"), "{plan}");
    assert_eq!(
        rows(
            &mut s,
            "SELECT count(*) FROM (SELECT id FROM docs ORDER BY \
             convex(score(body @@ 'w7'), emb <=> '[1,2,3,4]', 0.5) DESC LIMIT 10) x"
        )[0][0],
        Value::Int(10)
    );
}

#[test]
fn refused_where_it_cannot_rank() {
    let mut s = Session::new(db());
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, a FLOAT, b FLOAT)")
        .unwrap();
    s.execute("INSERT INTO t VALUES (1, 1, 2)").unwrap();
    // No limit: nothing to rank the first rows of.
    let e = s
        .execute("SELECT id FROM t ORDER BY rrf(a, b) DESC")
        .unwrap_err();
    assert!(
        format!("{e:?}").contains("ranks a query's first rows"),
        "{e:?}"
    );
    // Two plain rankings, greatest first.
    let r = rows(
        &mut s,
        "SELECT id, rrf(a, b) FROM t ORDER BY rrf(a, b) DESC LIMIT 1",
    );
    assert!((float(&r[0][1]) - 2.0 / 61.0).abs() < 1e-12, "{r:?}");
}
