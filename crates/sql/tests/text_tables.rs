//! The text and sparse indexes on every kind of table (PLAN.md §M14.21
//! T8): a `column` table, a `row,column` one and a table partitioned into
//! partitions of each storage give what a `row` table of the same rows
//! gives — the rows `@@` finds, the first by BM25 and their scores (a
//! partitioned table's statistics its partitions' together), highlights,
//! the nearest by `<#>`, `rrf` and `convex` — through inserts, updates,
//! deletes, rows moved between partitions and seals, each served by the
//! indexes.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    /// Words with stems in common, a stop word, of a skewed frequency.
    fn text(&mut self) -> String {
        const WORDS: [&str; 14] = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "running",
            "runs", "the", "kappa", "lambda", "mu",
        ];
        let n = 1 + self.below(12);
        (0..n)
            .map(|_| {
                let x = self.below(WORDS.len() as u64);
                WORDS[(x * x / WORDS.len() as u64) as usize]
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A sparse vector of 100 dimensions, the low ones common.
    fn vector(&mut self) -> String {
        let n = 1 + self.below(8);
        let mut at: Vec<u64> = (0..n)
            .map(|_| {
                let x = self.below(100);
                x * x / 100
            })
            .collect();
        at.sort_unstable();
        at.dedup();
        let parts: Vec<String> = at
            .iter()
            .map(|d| format!("{}:{}", d + 1, (1 + self.below(30)) as f64 / 10.0))
            .collect();
        format!("{{{}}}/100", parts.join(","))
    }
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The tables, the first of `row` storage, the one the others are held to.
const TABLES: [&str; 4] = ["r", "c", "rc", "p"];

const QUERIES: [&str; 8] = [
    "alpha",
    "beta gamma",
    "run",
    "\"alpha beta\"",
    "(delta OR zeta) AND NOT alpha",
    "lam*",
    "mu kappa lambda",
    "nothing",
];

const VECTORS: [&str; 3] = ["{1:1}/100", "{1:0.5,2:1.5,5:2}/100", "{3:1,40:2,70:3}/100"];

fn create(s: &mut Session) {
    let cols = "(id INT PRIMARY KEY, n INT, body TEXT, sv sparsevec(100))";
    for sql in [
        format!("CREATE TABLE r {cols}"),
        format!("CREATE TABLE c {cols} WITH (storage = column)"),
        format!("CREATE TABLE rc {cols} WITH (storage = 'row,column')"),
        format!("CREATE TABLE p {cols} PARTITION BY RANGE (id)"),
        "CREATE TABLE p_row PARTITION OF p FOR VALUES FROM (MINVALUE) TO (1000)".into(),
        "CREATE TABLE p_column PARTITION OF p FOR VALUES FROM (1000) TO (6000) \
         WITH (storage = column)"
            .into(),
        "CREATE TABLE p_both PARTITION OF p FOR VALUES FROM (6000) TO (MAXVALUE) \
         WITH (storage = 'row,column')"
            .into(),
    ] {
        s.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

/// The same writes to every table.
fn each(s: &mut Session, sql: &str) {
    for t in TABLES {
        let sql = sql.replace("{t}", t);
        s.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

fn insert(s: &mut Session, r: &mut Lcg, ids: std::ops::Range<i64>) {
    let values: Vec<String> = ids
        .map(|id| {
            let body = match id % 41 {
                7 => "NULL".to_string(),
                13 => "'the'".to_string(),
                _ => format!("'{}'", r.text()),
            };
            let sv = match id % 37 {
                5 => "NULL".to_string(),
                _ => format!("'{}'", r.vector()),
            };
            format!("({id}, {}, {body}, {sv})", r.below(100))
        })
        .collect();
    each(s, &format!("INSERT INTO {{t}} VALUES {}", values.join(",")));
}

/// Every query on every table as on the `row` one, served by the indexes.
fn same(s: &mut Session, step: &str) {
    let alike = |s: &mut Session, sql: &str, served: Option<&str>| {
        let want = rows(s, &sql.replace("{t}", TABLES[0]));
        for t in TABLES {
            let sql = sql.replace("{t}", t);
            assert_eq!(rows(s, &sql), want, "{step}: {sql}");
            if let Some(shown) = served {
                let plan = rows(s, &format!("EXPLAIN {sql}")).join("\n");
                assert!(
                    plan.contains(&shown.replace("{t}", t)),
                    "{step}: {sql}\n{plan}"
                );
            }
        }
        want
    };
    let mut found = 0;
    for q in QUERIES {
        let ids = alike(
            s,
            &format!("SELECT id FROM {{t}} WHERE body @@ '{q}' ORDER BY id"),
            Some("Index Scan using {t}_body"),
        );
        found += usize::from(!ids.is_empty());
        for filter in ["", " AND n % 10 = 3", " AND id < 1000"] {
            alike(
                s,
                &format!(
                    "SELECT id, score(body @@ '{q}') FROM {{t}} WHERE body @@ '{q}'{filter} \
                     ORDER BY score(body @@ '{q}') DESC, id LIMIT 7"
                ),
                // A key's range may be the cheaper way to rows of a few.
                (!filter.contains("id")).then_some(" first 7 by text_score(body) DESC with values"),
            );
        }
        alike(
            s,
            &format!(
                "SELECT id, highlight(body @@ '{q}'), snippet(body @@ '{q}') FROM {{t}} \
                 WHERE body @@ '{q}' ORDER BY id LIMIT 5"
            ),
            None,
        );
        for v in VECTORS {
            for (fusion, served) in [
                (
                    format!("rrf(score(body @@ '{q}'), sv <#> '{v}')"),
                    " first 100 by text_score",
                ),
                (
                    format!("convex(score(body @@ '{q}'), sv <#> '{v}', 0.3)"),
                    "100 nearest sv",
                ),
            ] {
                alike(
                    s,
                    &format!("SELECT id, {fusion} FROM {{t}} ORDER BY {fusion} DESC, id LIMIT 10"),
                    Some(served),
                );
            }
        }
    }
    assert!(
        found > QUERIES.len() / 2,
        "{step}: {found} queries find rows"
    );
    for v in VECTORS {
        for (filter, k) in [
            ("", 1),
            ("", 20),
            (" WHERE n % 10 = 3", 5),
            (" WHERE id >= 5000", 5),
        ] {
            alike(
                s,
                &format!(
                    "SELECT id, sv <#> '{v}' FROM {{t}}{filter} ORDER BY sv <#> '{v}', id \
                     LIMIT {k}"
                ),
                (!filter.contains("id"))
                    .then(|| format!("{k} nearest sv"))
                    .as_deref(),
            );
        }
    }
}

fn problems(kv: &Arc<dyn Kv>, s: &Session) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

#[test]
fn every_kind_of_table_alike() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(17)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(kv.clone());
    create(&mut s);
    let mut r = Lcg(5);
    // A load of segments where a table keeps them, the indexes built over
    // it.
    insert(&mut s, &mut r, 0..9000);
    each(
        &mut s,
        "CREATE INDEX {t}_body ON {t} USING text (body) WITH (analyzer = 'english')",
    );
    each(&mut s, "CREATE INDEX {t}_sv ON {t} USING sparse (sv)");
    each(&mut s, "ANALYZE {t}");
    same(&mut s, "built");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    // Writes: new rows, changed ones, gone ones, rows moved to another
    // partition.
    insert(&mut s, &mut r, 9000..9400);
    each(
        &mut s,
        "UPDATE {t} SET body = 'alpha beta running', sv = '{1:3,2:3}/100' WHERE id % 43 = 0",
    );
    each(&mut s, "UPDATE {t} SET id = id + 20000 WHERE id % 59 = 1");
    each(
        &mut s,
        "UPDATE {t} SET id = id - 7000 WHERE id BETWEEN 6000 AND 6100",
    );
    each(&mut s, "DELETE FROM {t} WHERE id % 29 = 0");
    same(&mut s, "written");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    same(&mut s, "sealed");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}
