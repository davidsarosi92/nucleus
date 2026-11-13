//! Permissioned retrieval, checked (PLAN.md §M14.24 R3): every way a
//! model's context is retrieved — nearest vectors through HNSW, BM25
//! through the text index, their fusions, `recall()`, `REMEMBER`'s merge —
//! returns only rows the role may see, and all of those a filter written
//! out would give; and BM25's statistics are a tenant's own on a table
//! partitioned by tenant, so its scores do not depend on other tenants'
//! rows.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

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

fn texts(s: &mut Session, sql: &str) -> Vec<String> {
    rows(s, sql)
        .into_iter()
        .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
        .collect()
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn unit(&mut self) -> f32 {
        (self.next() % 20_000) as f32 / 10_000.0 - 1.0
    }

    fn vector(&mut self, dims: usize) -> String {
        let c: Vec<String> = (0..dims).map(|_| format!("{:.4}", self.unit())).collect();
        format!("[{}]", c.join(","))
    }

    fn words(&mut self, n: usize) -> String {
        const WORDS: [&str; 16] = [
            "apple", "river", "stone", "cloud", "ember", "maple", "orbit", "quartz", "tulip",
            "violet", "willow", "zephyr", "harbor", "lantern", "meadow", "nectar",
        ];
        (0..n)
            .map(|_| WORDS[self.next() as usize % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

const TENANTS: [&str; 3] = ["acme", "globex", "initech"];

/// `docs`, of three tenants — partitioned by tenant or not —, with a text
/// and an HNSW index, under a policy that lets `app` read its tenant's
/// rows (`app.tenant`).
fn corpus(s: &mut Session, g: &mut Lcg, partitioned: bool) {
    let partitions = if partitioned {
        " PARTITION BY LIST (tenant); \
         CREATE TABLE docs_acme PARTITION OF docs FOR VALUES IN ('acme'); \
         CREATE TABLE docs_globex PARTITION OF docs FOR VALUES IN ('globex'); \
         CREATE TABLE docs_initech PARTITION OF docs FOR VALUES IN ('initech')"
    } else {
        ""
    };
    s.execute(&format!(
        "CREATE TABLE docs (id int, tenant text, body text, emb vector(8), \
             PRIMARY KEY (tenant, id)){partitions}; \
         CREATE USER app; GRANT SELECT ON docs TO app; \
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY tenant ON docs TO app USING (tenant = current_setting('app.tenant'))"
    ))
    .unwrap();
    let r: Vec<String> = (0..900)
        .map(|i| {
            // Uneven tenants: one has most of the rows.
            let tenant = TENANTS[[0, 1, 1, 1, 2][i % 5]];
            let n = 3 + g.next() as usize % 8;
            format!("({i}, '{tenant}', '{}', '{}')", g.words(n), g.vector(8))
        })
        .collect();
    s.execute(&format!("INSERT INTO docs VALUES {}", r.join(", ")))
        .unwrap();
    s.execute(
        "CREATE INDEX docs_body ON docs USING text (body); \
         CREATE INDEX docs_emb ON docs USING hnsw (emb); \
         ANALYZE",
    )
    .unwrap();
}

/// Each way of retrieving, as the role and as the administrator with the
/// policy written out: the same rows, all of the tenant's.
fn every_path_is_the_policy_s(partitioned: bool) {
    let mut s = Session::new(db(41));
    let mut g = Lcg(5);
    corpus(&mut s, &mut g, partitioned);
    let queries: Vec<(String, String)> = (0..12).map(|_| (g.words(1), g.vector(8))).collect();
    for tenant in TENANTS {
        for (q, v) in &queries {
            let paths = [
                // Nearest vectors (the index's order; ties by id).
                format!("SELECT id FROM docs {{w}} ORDER BY emb <-> '{v}', id LIMIT 10"),
                // BM25 ranked by the index.
                format!(
                    "SELECT id FROM docs {{w}} {{and}} body @@ '{q}' \
                     ORDER BY score(body @@ '{q}') DESC, id LIMIT 10"
                ),
                // Matches, counted.
                format!("SELECT count(*) FROM docs {{w}} {{and}} body @@ '{q}'"),
                // Both, fused.
                format!(
                    "SELECT id FROM docs {{w}} \
                     ORDER BY rrf(score(body @@ '{q}'), emb <=> '{v}'), id LIMIT 10"
                ),
                format!(
                    "SELECT id FROM docs {{w}} \
                     ORDER BY convex(score(body @@ '{q}'), emb <=> '{v}', 0.5), id LIMIT 10"
                ),
            ];
            for path in &paths {
                let as_admin = path
                    .replace("{w}", &format!("WHERE tenant = '{tenant}'"))
                    .replace("{and}", "AND");
                let as_app = path.replace("{w} {and}", "WHERE").replace("{w}", "");
                let want = texts(&mut s, &as_admin);
                s.execute(&format!("SET app.tenant = '{tenant}'; SET ROLE app"))
                    .unwrap();
                let got = texts(&mut s, &as_app);
                s.execute("RESET ROLE").unwrap();
                // The same rows, in the same order: none of another
                // tenant's, none of its own missing.
                assert_eq!(got, want, "{tenant}: {as_app}");
                if as_app.starts_with("SELECT id") {
                    assert!(!got.is_empty(), "{tenant}: {as_app}");
                    assert_eq!(
                        texts(
                            &mut s,
                            &format!(
                                "SELECT count(*) FROM docs WHERE id IN ({}) AND tenant <> '{tenant}'",
                                got.join(",")
                            )
                        ),
                        ["0"]
                    );
                }
            }
        }
    }
}

#[test]
fn every_recall_path_returns_the_policy_s_rows_on_a_table() {
    every_path_is_the_policy_s(false);
}

#[test]
fn every_recall_path_returns_the_policy_s_rows_on_a_partitioned_table() {
    every_path_is_the_policy_s(true);
}

#[test]
fn a_tenant_s_bm25_scores_are_its_own_on_a_table_partitioned_by_tenant() {
    let mut s = Session::new(db(43));
    let mut g = Lcg(7);
    corpus(&mut s, &mut g, true);
    // The same rows alone in a table of their own: the scores to match.
    s.execute(
        "CREATE TABLE alone (id int PRIMARY KEY, body text); \
         INSERT INTO alone SELECT id, body FROM docs WHERE tenant = 'acme'; \
         CREATE INDEX ON alone USING text (body)",
    )
    .unwrap();
    let ranked = |s: &mut Session, table: &str| {
        texts(
            s,
            &format!(
                "SELECT id, round(score(body @@ 'apple river')::numeric, 6) FROM {table} \
                 WHERE body @@ 'apple river' ORDER BY 2 DESC, id LIMIT 20"
            ),
        )
    };
    let alone = ranked(&mut s, "alone");
    assert!(alone.len() >= 10);
    let as_app = |s: &mut Session| {
        s.execute("SET app.tenant = 'acme'; SET ROLE app").unwrap();
        let r = ranked(s, "docs");
        s.execute("RESET ROLE").unwrap();
        r
    };
    assert_eq!(as_app(&mut s), alone);
    // The administrator, reading every partition, scores by all of them.
    let admin = texts(
        &mut s,
        "SELECT id, round(score(body @@ 'apple river')::numeric, 6) FROM docs \
         WHERE tenant = 'acme' AND body @@ 'apple river' ORDER BY 2 DESC, id LIMIT 20",
    );
    assert_ne!(admin, alone);
    // Another tenant's rows, however many and whatever they hold, do not
    // move acme's scores.
    s.execute(
        "INSERT INTO docs SELECT 10000 + g, 'globex', 'apple apple apple', '[0,0,0,0,0,0,0,1]' \
         FROM generate_series(1, 500) g",
    )
    .unwrap();
    assert_eq!(as_app(&mut s), alone);
    // Through a fusion too.
    s.execute("SET app.tenant = 'acme'; SET ROLE app").unwrap();
    let fused = texts(
        &mut s,
        "SELECT id FROM docs ORDER BY rrf(score(body @@ 'apple'), emb <=> '[1,0,0,0,0,0,0,0]'), id LIMIT 5",
    );
    s.execute("RESET ROLE").unwrap();
    assert_eq!(fused.len(), 5);
}

#[test]
fn recall_and_remember_stay_within_a_subject() {
    let mut s = Session::new(db(47));
    let mut g = Lcg(11);
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 32); \
         CREATE MEMORY STORE notes USING h; \
         SET embedding.pending = wait",
    )
    .unwrap();
    let subjects = ["u1", "u2", "u3"];
    for i in 0..90 {
        let who = subjects[i % 3];
        s.execute(&format!(
            "REMEMBER '{who} {}' FOR '{who}' IN notes",
            g.words(5)
        ))
        .unwrap();
    }
    for _ in 0..20 {
        let q = g.words(2);
        for who in subjects {
            let got = texts(
                &mut s,
                &format!("SELECT content FROM recall('notes', '{who}', '{q}', 50)"),
            );
            // (Each memory's text begins with its subject.)
            assert!(!got.is_empty());
            assert!(
                got.iter().all(|x| x.starts_with(&format!("{who} "))),
                "{who} {q}: {got:?}"
            );
        }
    }
    // The same text for another subject is a memory of its own.
    let text = texts(
        &mut s,
        "SELECT content FROM notes WHERE subject = 'u1' LIMIT 1",
    )
    .remove(0)
    .replace('\'', "''");
    let did = texts(&mut s, &format!("REMEMBER '{text}' FOR 'u2' IN notes"));
    assert!(did[0].ends_with("inserted"), "{did:?}");
}
