//! The sparse vector index (PLAN.md §M14.21 T7b): `ORDER BY sv <#> q, id
//! LIMIT k` ranked by the index held to the same order computed on every
//! row — with filters, after inserts, updates and deletes (the delta),
//! seals and merges; the queries it cannot rank (a weight not positive,
//! fewer than `k` rows sharing a dimension) read the rows instead.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
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

    /// A sparse vector of 200 dimensions: a few components, the low
    /// dimensions common (a word's distribution), a tenth negative, their
    /// weights with ties.
    fn vector(&mut self) -> String {
        let n = 1 + self.below(12);
        let mut at: Vec<u64> = (0..n)
            .map(|_| {
                let x = self.below(200);
                x * x / 200
            })
            .collect();
        at.sort_unstable();
        at.dedup();
        let parts: Vec<String> = at
            .iter()
            .map(|d| {
                let w = (1 + self.below(30)) as f64 / 10.0;
                let w = if self.below(10) == 0 { -w } else { w };
                format!("{}:{w}", d + 1)
            })
            .collect();
        format!("{{{}}}/200", parts.join(","))
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

const QUERIES: [&str; 6] = [
    "{1:1}/200",
    "{1:0.5,2:1.5,5:2}/200",
    "{3:1,40:2,100:0.25,150:3}/200",
    "{1:1,2:1,3:1,4:1,5:1,6:1,7:1,8:1}/200",
    // A weight not positive, and dimensions no row has: the rows are read.
    "{1:1,2:-1}/200",
    "{199:1}/200",
];

/// Every query, ranked by the index and on the rows, alike.
fn same(s: &mut Session, step: &str) {
    for (n, q) in QUERIES.iter().enumerate() {
        for k in [1, 5, 20] {
            for filter in ["", " WHERE n % 2 = 0", " WHERE n % 10 = 3"] {
                let sql = |by: &str| {
                    format!(
                        "SELECT id, sv <#> '{q}' FROM docs{filter} \
                         ORDER BY (sv <#> '{q}'){by}, id LIMIT {k}"
                    )
                };
                let plan = rows(s, &format!("EXPLAIN {}", sql(""))).join("\n");
                if n < 4 && filter.is_empty() {
                    assert!(plan.contains("using docs_sv"), "{step}: {q}\n{plan}");
                }
                assert_eq!(
                    rows(s, &sql("")),
                    rows(s, &sql(" + 0")),
                    "{step}: {q} {filter} {k}"
                );
            }
        }
    }
}

fn problems(kv: &Arc<dyn Kv>, s: &Session) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

#[test]
fn ranked_as_every_row_through_writes_seals_and_merges() {
    let kv = db();
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE docs (id INT PRIMARY KEY, n INT, sv sparsevec(200))")
        .unwrap();
    let mut r = Lcg(3);
    let insert = |s: &mut Session, r: &mut Lcg, ids: std::ops::Range<i64>| {
        let values: Vec<String> = ids
            .map(|id| {
                let sv = if id % 50 == 7 {
                    "NULL".to_string()
                } else {
                    format!("'{}'", r.vector())
                };
                format!("({id}, {}, {sv})", r.below(100))
            })
            .collect();
        s.execute(&format!("INSERT INTO docs VALUES {}", values.join(",")))
            .unwrap();
    };
    insert(&mut s, &mut r, 0..600);
    s.execute("CREATE INDEX docs_sv ON docs USING sparse (sv)")
        .unwrap();
    s.execute("ANALYZE docs").unwrap();
    same(&mut s, "built");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    // Writes in the delta: new rows, changed ones, gone ones.
    insert(&mut s, &mut r, 600..900);
    s.execute("UPDATE docs SET sv = '{1:3,2:3}/200' WHERE id % 37 = 0")
        .unwrap();
    s.execute("DELETE FROM docs WHERE id % 29 = 0").unwrap();
    same(&mut s, "delta");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    same(&mut s, "sealed");
    insert(&mut s, &mut r, 900..1000);
    s.execute("DELETE FROM docs WHERE id % 31 = 0").unwrap();
    nucleus_sql::text::index::upkeep_now(&kv).unwrap();
    {
        let txn = kv.begin(TxnOptions::read_only()).unwrap();
        let (t, i) = nucleus_sql::catalog::find_index(&*txn, "docs_sv")
            .unwrap()
            .unwrap();
        drop(txn);
        nucleus_sql::text::index::merge_all(&kv, &t, i).unwrap();
    }
    same(&mut s, "merged");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

#[test]
fn refused_where_it_does_not_fit() {
    let mut s = Session::new(db());
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, v vector(3), sv sparsevec(5))")
        .unwrap();
    for (sql, code) in [
        ("CREATE INDEX t_v ON t USING sparse (v)", "0A000"),
        ("CREATE INDEX t_s ON t USING sparse (id, sv)", "0A000"),
        (
            "CREATE INDEX t_s ON t USING sparse (sv) WITH (m = 4)",
            "22023",
        ),
    ] {
        let e = s.execute(sql).unwrap_err();
        assert!(format!("{e:?}").contains(code), "{sql}: {e:?}");
    }
    s.execute("CREATE INDEX t_s ON t USING sparse (sv sparsevec_ip_ops)")
        .unwrap();
}
