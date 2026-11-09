//! HNSW as an index method (PLAN.md §M14.12 V4b): built over a table's
//! rows, kept on inserts, updates and deletes (a delta, sealed and folded
//! by the upkeep), searched — its answers held to the exact search's —
//! through rollbacks, the upkeep's consolidation of deleted rows, and the
//! integrity check; and through the planner, `ORDER BY col <op> q LIMIT k`.

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

/// Deterministic vectors in [-1, 1).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn vector(&mut self, dims: usize) -> String {
        let c: Vec<String> = (0..dims).map(|_| format!("{:.4}", self.next())).collect();
        format!("[{}]", c.join(","))
    }
}

fn floats(s: &mut Session, sql: &str) -> Vec<f64> {
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

fn problems(kv: &Arc<dyn Kv>, s: &Session) -> Vec<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

/// The index's `k` nearest distances from `q`.
fn nearest(kv: &Arc<dyn Kv>, index: &str, q: &str, k: usize, ef: usize) -> Vec<f64> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = nucleus_sql::catalog::find_index(&*txn, index)
        .unwrap()
        .unwrap();
    let q = nucleus_sql::vectors::value(&nucleus_sql::vectors::parse(q).unwrap());
    nucleus_sql::hnsw::nearest(&*txn, &t, i, &q, k, ef, 4096)
        .unwrap()
        .into_iter()
        .map(|h| h.2)
        .collect()
}

/// What the index holds: nodes, dead nodes, rows sealed, delta keys.
fn status(kv: &Arc<dyn Kv>, index: &str) -> (u32, usize, u64, usize) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let (t, i) = nucleus_sql::catalog::find_index(&*txn, index)
        .unwrap()
        .unwrap();
    let st = nucleus_sql::hnsw::status(&*txn, &t, i).unwrap();
    (st.nodes, st.dead, st.sealed, st.delta)
}

/// The share of the exact `k` nearest the index found (by distance).
fn recall(found: &[f64], exact: &[f64]) -> f64 {
    let Some(&kth) = exact.last() else {
        return 1.0;
    };
    let hits = found.iter().filter(|&&d| d <= kth + 1e-9).count();
    hits.min(exact.len()) as f64 / exact.len() as f64
}

fn mean_recall(
    kv: &Arc<dyn Kv>,
    s: &mut Session,
    op: &str,
    index: &str,
    queries: &[String],
    ef: usize,
) -> f64 {
    let mut sum = 0.0;
    for q in queries {
        let exact = floats(
            s,
            &format!(
                "SELECT (v {op} '{q}') + 0 FROM items WHERE v IS NOT NULL ORDER BY 1 LIMIT 10"
            ),
        );
        let found = nearest(kv, index, q, 10, ef);
        assert!(
            found.windows(2).all(|w| w[0] <= w[1]),
            "nearest first: {found:?}"
        );
        sum += recall(&found, &exact);
    }
    sum / queries.len() as f64
}

#[test]
fn hnsw_answers_as_the_exact_search_through_writes_and_upkeep() {
    let kv = db(11);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(7);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE items (id INT PRIMARY KEY, v vector({dims}), tag TEXT)"
    ))
    .unwrap();
    let rows: Vec<String> = (0..1500)
        .map(|i| format!("({i}, '{}', 't{}')", g.vector(dims), i % 7))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", rows.join(", ")))
        .unwrap();
    s.execute("INSERT INTO items VALUES (100000, NULL, 'none')")
        .unwrap();
    s.execute("CREATE INDEX items_v ON items USING hnsw (v vector_l2_ops) WITH (m = 8, ef_construction = 32)")
        .unwrap();
    let queries: Vec<String> = (0..40).map(|_| g.vector(dims)).collect();
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    assert_eq!(status(&kv, "items_v"), (1500, 0, 0, 0));
    let built = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(built > 0.95, "built: recall {built}");

    // Writes after the build go to the delta: found exactly.
    let more: Vec<String> = (1500..2000)
        .map(|i| format!("({i}, '{}', 'new')", g.vector(dims)))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", more.join(", ")))
        .unwrap();
    s.execute("UPDATE items SET v = v + '[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1]' WHERE id % 10 = 0")
        .unwrap();
    s.execute("UPDATE items SET tag = 'x' WHERE id % 10 = 1")
        .unwrap();
    s.execute("DELETE FROM items WHERE id % 10 = 2").unwrap();
    // Rolled back: never found.
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO items VALUES (200000, '[0,0,0,0,0,0,0,0]', 'gone')")
        .unwrap();
    s.execute("ROLLBACK").unwrap();
    let origin = "[0,0,0,0,0,0,0,0]";
    let exact = floats(
        &mut s,
        &format!(
            "SELECT (v <-> '{origin}') + 0 FROM items WHERE v IS NOT NULL ORDER BY 1 LIMIT 10"
        ),
    );
    assert_eq!(nearest(&kv, "items_v", origin, 10, 40).len(), 10);
    assert!(recall(&nearest(&kv, "items_v", origin, 10, 40), &exact) > 0.8);
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    let before = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(before > 0.9, "with a delta: recall {before}");
    // 500 rows inserted, 200 updated, 200 deleted: 1,100 keys (the
    // rolled-back row's gone with its transaction).
    assert_eq!(status(&kv, "items_v"), (1500, 0, 0, 1100));

    // Sealed and folded: the updated and deleted rows' old nodes marked
    // dead, the new rows in the graph.
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    // Folded: 400 of the inserted rows (the other 100 updated or deleted
    // since) and the 200 updated ones; the 300 graph nodes of rows updated
    // or deleted, past 2 % of the graph, cut out at once.
    assert_eq!(status(&kv, "items_v"), (2100, 0, 0, 0));
    let folded = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(folded > 0.9, "folded: recall {folded}");

    // A snapshot from before an upkeep still reads what it saw.
    let old = kv.begin(TxnOptions::read_only()).unwrap();
    s.execute("DELETE FROM items WHERE id % 10 = 3").unwrap();
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    {
        let (t, i) = nucleus_sql::catalog::find_index(&*old, "items_v")
            .unwrap()
            .unwrap();
        let q = nucleus_sql::vectors::value(&nucleus_sql::vectors::parse(origin).unwrap());
        let found = nucleus_sql::hnsw::nearest(&*old, &t, i, &q, 10, 40, 4096).unwrap();
        assert_eq!(found.len(), 10);
    }
    drop(old);
    // 200 rows gone of 2,100 nodes: marked, and past 2 %: cut out.
    assert_eq!(status(&kv, "items_v"), (2100, 0, 0, 0));
    // 20 gone: marked dead, passed over, kept until there are more.
    s.execute("DELETE FROM items WHERE id % 100 = 5").unwrap();
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    assert_eq!(status(&kv, "items_v"), (2100, 20, 0, 0));
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    let marked = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(marked > 0.9, "marked: recall {marked}");
    // Enough deleted (past 2 % of the graph) that a fold cuts them out.
    s.execute("DELETE FROM items WHERE id % 10 = 4").unwrap();
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    let consolidated = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(consolidated > 0.9, "consolidated: recall {consolidated}");
    // A rebuilt table rebuilds the index.
    s.execute("ALTER TABLE items ALTER COLUMN tag TYPE VARCHAR(10)")
        .unwrap();
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    let rebuilt = mean_recall(&kv, &mut s, "<->", "items_v", &queries, 40);
    assert!(rebuilt > 0.9, "rebuilt: recall {rebuilt}");
}

#[test]
fn hnsw_operator_classes_answer_their_distances() {
    let kv = db(12);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(9);
    let dims = 6;
    s.execute(&format!(
        "CREATE TABLE items (id INT PRIMARY KEY, v vector({dims}))"
    ))
    .unwrap();
    let rows: Vec<String> = (0..800)
        .map(|i| format!("({i}, '{}')", g.vector(dims)))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", rows.join(", ")))
        .unwrap();
    // A zero vector has no direction: not in the cosine index.
    s.execute("INSERT INTO items VALUES (9999, '[0,0,0,0,0,0]')")
        .unwrap();
    let queries: Vec<String> = (0..20).map(|_| g.vector(dims)).collect();
    for (op, class) in [
        ("<=>", "vector_cosine_ops"),
        ("<#>", "vector_ip_ops"),
        ("<+>", "vector_l1_ops"),
    ] {
        let name = format!("items_{}", &class[7..class.len() - 4]);
        s.execute(&format!(
            "CREATE INDEX {name} ON items USING hnsw (v {class})"
        ))
        .unwrap();
        let more: Vec<String> = (0..50)
            .map(|i| format!("({}, '{}')", 10_000 + i, g.vector(dims)))
            .collect();
        s.execute(&format!("INSERT INTO items VALUES {}", more.join(", ")))
            .unwrap();
        let exact_filter = if op == "<=>" { " AND id <> 9999" } else { "" };
        let mut sum = 0.0;
        for q in &queries {
            let exact = floats(
                &mut s,
                &format!(
                    "SELECT (v {op} '{q}') + 0 FROM items WHERE v IS NOT NULL{exact_filter} ORDER BY 1 LIMIT 10"
                ),
            );
            sum += recall(&nearest(&kv, &name, q, 10, 64), &exact);
        }
        let r = sum / queries.len() as f64;
        assert!(r > 0.9, "{class}: recall {r}");
        s.execute("DELETE FROM items WHERE id >= 10000 AND id < 20000")
            .unwrap();
        assert_eq!(problems(&kv, &s), Vec::<String>::new(), "{class}");
    }
}

#[test]
fn hnsw_definitions_are_checked() {
    let kv = db(13);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, v vector(3), w vector, n INT)")
        .unwrap();
    for (sql, state) in [
        ("CREATE INDEX ON t USING hnsw (w)", "22023"),
        ("CREATE INDEX ON t USING hnsw (n)", "0A000"),
        ("CREATE INDEX ON t USING hnsw (v, n)", "0A000"),
        ("CREATE INDEX ON t USING hnsw (v int4_ops)", "42704"),
        ("CREATE INDEX ON t USING hnsw (v) WITH (m = 1)", "22023"),
        (
            "CREATE INDEX ON t USING hnsw (v) WITH (m = 40, ef_construction = 64)",
            "22023",
        ),
        ("CREATE INDEX ON t USING hnsw (v) WITH (lists = 4)", "22023"),
        ("CREATE UNIQUE INDEX ON t USING hnsw (v)", "0A000"),
    ] {
        assert_eq!(s.execute(sql).expect_err(sql).code, state, "{sql}");
    }
    s.execute("CREATE INDEX t_v ON t USING hnsw (v)").unwrap();
    match s
        .execute("SELECT opclasses, options FROM nc_catalog.indexes WHERE name = 't_v'")
        .unwrap()
        .pop()
    {
        Some(Output::Rows { rows, .. }) => {
            let r: Vec<String> = rows[0].iter().map(|v| v.render()).collect();
            assert_eq!(r, ["{vector_l2_ops}", "{m=16,ef_construction=64}"]);
        }
        other => panic!("{other:?}"),
    }
    // An empty index, written to, searched; its column's type kept.
    s.execute("INSERT INTO t VALUES (1, '[1,0,0]', NULL, 1), (2, '[0,1,0]', NULL, 2)")
        .unwrap();
    let found = nearest(&kv, "t_v", "[1,0.1,0]", 1, 10);
    assert!(
        found.len() == 1 && (found[0] - 0.1).abs() < 1e-6,
        "{found:?}"
    );
    assert_eq!(
        s.execute("ALTER TABLE t ALTER COLUMN v TYPE TEXT")
            .expect_err("a text column")
            .code,
        "0A000"
    );
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

#[test]
fn hnsw_quantized_indexes_answer_by_the_rows_distances() {
    let kv = db(15);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(21);
    let dims = 24;
    s.execute(&format!(
        "CREATE TABLE items (id INT PRIMARY KEY, v vector({dims}))"
    ))
    .unwrap();
    let rows: Vec<String> = (0..1500)
        .map(|i| format!("({i}, '{}')", g.vector(dims)))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", rows.join(", ")))
        .unwrap();
    let queries: Vec<String> = (0..20).map(|_| g.vector(dims)).collect();
    // Bits say little of 24 components: a binary index reads many rows
    // (it is meant for embeddings of hundreds and thousands).
    for (codec, rescore, least) in [("half", 1, 0.95), ("int8", 2, 0.95), ("binary", 30, 0.8)] {
        for (op, class) in [
            ("<->", "vector_l2_ops"),
            ("<=>", "vector_cosine_ops"),
            ("<#>", "vector_ip_ops"),
            ("<+>", "vector_l1_ops"),
        ] {
            let name = format!("items_{codec}_{}", &class[7..class.len() - 4]);
            s.execute(&format!(
                "CREATE INDEX {name} ON items USING hnsw (v {class}) \
                 WITH (quantize = '{codec}', rescore = {rescore})"
            ))
            .unwrap();
            // Rows after the build: in the delta, then folded.
            let more: Vec<String> = (0..100)
                .map(|i| format!("({}, '{}')", 10_000 + i, g.vector(dims)))
                .collect();
            s.execute(&format!("INSERT INTO items VALUES {}", more.join(", ")))
                .unwrap();
            let mut sum = 0.0;
            for q in &queries {
                let exact = floats(
                    &mut s,
                    &format!("SELECT (v {op} '{q}') + 0 FROM items ORDER BY 1 LIMIT 10"),
                );
                let found = nearest(&kv, &name, q, 10, 40);
                // The distances are the rows' own, exactly: the codes only
                // choose which rows are read.
                for d in &found {
                    assert!(
                        exact.iter().any(|e| e == d)
                            || floats(
                                &mut s,
                                &format!(
                                    "SELECT count(*) FROM items WHERE (v {op} '{q}') + 0 = {d}"
                                ),
                            ) == [1.0],
                        "{name}: {d} is no row's distance"
                    );
                }
                sum += recall(&found, &exact);
            }
            let r = sum / queries.len() as f64;
            assert!(r >= least, "{name}: recall {r}");
            s.execute("DELETE FROM items WHERE id >= 10000").unwrap();
            assert_eq!(problems(&kv, &s), Vec::<String>::new(), "{name}");
            s.execute(&format!("DROP INDEX {name}")).unwrap();
        }
    }
    // The options, normalized; the wrong ones refused.
    s.execute("CREATE INDEX items_q ON items USING hnsw (v) WITH (quantize = 'INT8')")
        .unwrap();
    match s
        .execute("SELECT options FROM nc_catalog.indexes WHERE name = 'items_q'")
        .unwrap()
        .pop()
    {
        Some(Output::Rows { rows, .. }) => assert_eq!(
            rows[0][0].render(),
            "{m=16,ef_construction=64,quantize=int8,rescore=2}"
        ),
        other => panic!("{other:?}"),
    }
    for sql in [
        "CREATE INDEX ON items USING hnsw (v) WITH (quantize = 'int4')",
        "CREATE INDEX ON items USING hnsw (v) WITH (rescore = 2)",
        "CREATE INDEX ON items USING hnsw (v) WITH (quantize = 'half', rescore = 0)",
    ] {
        assert_eq!(s.execute(sql).expect_err(sql).code, "22023", "{sql}");
    }
    // Through the planner: the answer ordered by the rows' distances.
    let q = &queries[0];
    let sql = format!("SELECT id FROM items ORDER BY v <-> '{q}' LIMIT 5");
    assert!(
        plan(&mut s, &sql).contains("nearest v to"),
        "{}",
        plan(&mut s, &sql)
    );
    let found = floats(
        &mut s,
        &format!("SELECT (v <-> '{q}') + 0 FROM ({sql}) f JOIN items USING (id) ORDER BY 1"),
    );
    let exact = floats(
        &mut s,
        &format!("SELECT (v <-> '{q}') + 0 FROM items ORDER BY 1 LIMIT 5"),
    );
    assert!(recall(&found, &exact) >= 0.8, "{found:?} {exact:?}");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

#[test]
fn hnsw_upkeep_runs_after_commits_and_searches() {
    let kv = db(14);
    let mut s = Session::new(kv.clone());
    // Table ids apart from the other tests' (the upkeep's notes are the
    // process's).
    for n in 0..7 {
        s.execute(&format!("CREATE TABLE pad{n} (x INT)")).unwrap();
    }
    s.execute("CREATE TABLE pts (id INT PRIMARY KEY, v vector(4))")
        .unwrap();
    s.execute("CREATE INDEX pts_v ON pts USING hnsw (v)")
        .unwrap();
    let mut g = Lcg(5);
    for b in 0..3 {
        let rows: Vec<String> = (0..4000)
            .map(|i| format!("({}, '{}')", b * 4000 + i, g.vector(4)))
            .collect();
        s.execute(&format!("INSERT INTO pts VALUES {}", rows.join(", ")))
            .unwrap();
    }
    // No search yet: waiting rows cost nothing, so the sizes are their
    // most (10,000, nudged a fifth either way so a fold's cost can be
    // told from its rows') — a commit past them sealed and folded.
    let (nodes, dead, sealed, delta) = status(&kv, "pts_v");
    assert!(nodes >= 8000 && dead == 0, "{nodes} nodes, {dead} dead");
    assert_eq!(u64::from(nodes) + sealed + delta as u64, 12_000);
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
    // Writes stopped, searches ask: the rows left waiting cost each
    // search, and the sizes fall with the write rate — the searches'
    // sessions fold them.
    s.execute("INSERT INTO pts VALUES (99999, '[0,0,0,0]')")
        .unwrap();
    let mut folded = false;
    for _ in 0..200 {
        let near = rows(
            &mut s,
            "SELECT id FROM pts ORDER BY v <-> '[0,0,0,0]' LIMIT 5",
        );
        assert_eq!(near[0][0], "99999");
        let (_, _, sealed, delta) = status(&kv, "pts_v");
        if sealed == 0 && delta == 0 {
            folded = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(folded, "still waiting: {:?}", status(&kv, "pts_v"));
    assert_eq!(status(&kv, "pts_v").0, 12_001);
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn plan(s: &mut Session, sql: &str) -> String {
    rows(s, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|r| r.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn hnsw_serves_order_by_distance_limit() {
    let kv = db(13);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(3);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE items (id INT PRIMARY KEY, v vector({dims}), tag TEXT)"
    ))
    .unwrap();
    let r: Vec<String> = (0..12000)
        .map(|i| format!("({i}, '{}', 't{}')", g.vector(dims), i % 7))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", r.join(", ")))
        .unwrap();
    s.execute("CREATE INDEX items_v ON items USING hnsw (v)")
        .unwrap();
    let queries: Vec<String> = (0..20).map(|_| g.vector(dims)).collect();
    let q = &queries[0];
    let searched = |s: &mut Session, sql: &str| plan(s, sql).contains("nearest v to");

    // Without statistics: the rows the index holds.
    let sql = format!("SELECT id FROM items ORDER BY v <-> '{q}' LIMIT 10");
    assert!(
        plan(&mut s, &sql).contains("Index Scan using items_v on items (10 nearest v to"),
        "{}",
        plan(&mut s, &sql)
    );
    // Another distance than the operator class's; the distance not the
    // first key, or descending; an expression of it: read and sorted.
    for sql in [
        format!("SELECT id FROM items ORDER BY v <=> '{q}' LIMIT 10"),
        format!("SELECT id FROM items ORDER BY tag, v <-> '{q}' LIMIT 10"),
        format!("SELECT id FROM items ORDER BY v <-> '{q}' DESC LIMIT 10"),
        format!("SELECT id FROM items ORDER BY (v <-> '{q}') + 0 LIMIT 10"),
        "SELECT id FROM items ORDER BY v <-> v LIMIT 10".to_string(),
        format!("SELECT id FROM items ORDER BY v <-> '{q}'"),
    ] {
        assert!(!searched(&mut s, &sql), "{sql}");
    }
    // Fewer rows pass than asked for: every row is read.
    let sql = format!("SELECT id FROM items WHERE id % 5000 = 0 ORDER BY v <-> '{q}' LIMIT 5");
    let mut ids = rows(&mut s, &sql);
    ids.sort();
    assert_eq!(ids, [["0"], ["10000"], ["5000"]]);
    // A key range the filter chose stays without statistics.
    let sql = format!("SELECT id FROM items WHERE id < 20 ORDER BY v <-> '{q}' LIMIT 5");
    assert!(!searched(&mut s, &sql), "{sql}");

    // The answers: the exact search's, nearly; with ef_search at its
    // most, the same distances.
    let distances = |s: &mut Session, filter: &str, k: usize, exact: bool| -> Vec<Vec<f64>> {
        let d = if exact {
            "(v <-> '{q}') + 0"
        } else {
            "v <-> '{q}'"
        };
        queries
            .iter()
            .map(|q| {
                let d = d.replace("{q}", q);
                floats(
                    s,
                    &format!("SELECT {d} AS d FROM items {filter} ORDER BY d LIMIT {k}"),
                )
            })
            .collect()
    };
    let mean = |found: &[Vec<f64>], exact: &[Vec<f64>]| {
        found
            .iter()
            .zip(exact)
            .map(|(f, e)| recall(f, e))
            .sum::<f64>()
            / found.len() as f64
    };
    let exact = distances(&mut s, "", 10, true);
    let found = distances(&mut s, "", 10, false);
    assert!(found.iter().all(|f| f.len() == 10));
    assert!(
        mean(&found, &exact) > 0.9,
        "recall {}",
        mean(&found, &exact)
    );
    s.execute("SET hnsw.ef_search = 1000").unwrap();
    for (f, e) in distances(&mut s, "", 10, false).iter().zip(&exact) {
        assert_eq!(f.len(), e.len());
        assert!(
            f.iter().zip(e).all(|(a, b)| (a - b).abs() < 1e-9),
            "{f:?} {e:?}"
        );
    }
    s.execute("RESET hnsw.ef_search").unwrap();
    let e = s.execute("SET hnsw.ef_search = 0").unwrap_err();
    assert_eq!(e.code, "22023");

    // With statistics, a filter: the index searched until k rows pass.
    s.execute("ANALYZE").unwrap();
    let filter = "WHERE tag = 't3'";
    let sql = format!("SELECT id FROM items {filter} ORDER BY v <-> '{q}' LIMIT 10");
    assert!(searched(&mut s, &sql), "{}", plan(&mut s, &sql));
    let exact = distances(&mut s, filter, 10, true);
    let found = distances(&mut s, filter, 10, false);
    assert!(found.iter().all(|f| f.len() == 10));
    assert!(
        mean(&found, &exact) > 0.9,
        "filtered: recall {}",
        mean(&found, &exact)
    );
    let tags = rows(
        &mut s,
        &format!("SELECT tag FROM items {filter} ORDER BY v <-> '{q}' LIMIT 10"),
    );
    assert!(tags.iter().all(|t| t[0] == "t3"), "{tags:?}");
    // A narrow key range: read and sorted.
    let sql = format!("SELECT id FROM items WHERE id < 20 ORDER BY v <-> '{q}' LIMIT 5");
    assert!(!searched(&mut s, &sql), "{sql}");
    // The offset counts: the rows past it are the first eight's last.
    let sql = format!("SELECT id FROM items {filter} ORDER BY v <-> '{q}' LIMIT 5 OFFSET 3");
    assert!(
        plan(&mut s, &sql).contains("(5 + 3) nearest"),
        "{}",
        plan(&mut s, &sql)
    );
    let first = rows(
        &mut s,
        &format!("SELECT id FROM items {filter} ORDER BY v <-> '{q}' LIMIT 8"),
    );
    assert_eq!(rows(&mut s, &sql), first[3..]);
    // An ef_search as costly as reading every row: read and sorted.
    s.execute("SET hnsw.ef_search = 1000").unwrap();
    assert!(!searched(&mut s, &sql), "{sql}");
    s.execute("RESET hnsw.ef_search").unwrap();

    // Fewer rows than asked for, one without a vector: it comes last.
    s.execute("CREATE TABLE few (id INT PRIMARY KEY, v vector(2))")
        .unwrap();
    s.execute("CREATE INDEX few_v ON few USING hnsw (v)")
        .unwrap();
    s.execute("INSERT INTO few VALUES (1, '[0,0]'), (2, '[1,1]'), (3, NULL), (4, '[2,2]')")
        .unwrap();
    let ids = rows(&mut s, "SELECT id FROM few ORDER BY v <-> '[0,0]' LIMIT 10");
    assert_eq!(ids, [["1"], ["2"], ["4"], ["3"]]);

    // The enclosing row's value: each row's nearest is itself.
    let sql = "SELECT a.id, (SELECT b.id FROM items b ORDER BY b.v <-> a.v LIMIT 1) \
               FROM items a WHERE a.id < 30 ORDER BY a.id";
    assert!(
        plan(&mut s, sql).contains("1 nearest v to"),
        "{}",
        plan(&mut s, sql)
    );
    for r in rows(&mut s, sql) {
        assert_eq!(r[0], r[1]);
    }

    // The query vector from a subquery ("more like this row", PLAN.md
    // §M14.12 V7): one run for the statement, searched; the row itself is
    // the nearest.
    let sql = "SELECT id FROM items ORDER BY v <-> (SELECT v FROM items WHERE id = 42) LIMIT 5";
    assert!(
        plan(&mut s, sql).contains("5 nearest v to"),
        "{}",
        plan(&mut s, sql)
    );
    assert_eq!(rows(&mut s, sql)[0][0], "42");
    let like = |s: &mut Session, sql: &str| {
        let mut r = rows(s, sql);
        r.sort();
        r
    };
    let exact =
        "SELECT id FROM items ORDER BY (v <-> (SELECT v FROM items WHERE id = 42)) + 0 LIMIT 5";
    assert_eq!(like(&mut s, sql), like(&mut s, exact));
    // No row: no vector, every row read (NULL distances, by id).
    let none =
        "SELECT id FROM items ORDER BY v <-> (SELECT v FROM items WHERE id = -1), id LIMIT 2";
    assert_eq!(rows(&mut s, none), [["0"], ["1"]]);
    // A subquery that reads the scan's own row is not one query: read and
    // sorted.
    let sql =
        "SELECT id FROM items i ORDER BY v <-> (SELECT b.v FROM items b WHERE b.id = i.id) LIMIT 5";
    assert!(!searched(&mut s, sql), "{}", plan(&mut s, sql));

    // LATERAL: each row's nearest through the index, a search per row.
    let sql = "SELECT a.id, n.id FROM items a, LATERAL (SELECT b.id FROM items b \
               WHERE b.id <> a.id ORDER BY b.v <-> a.v LIMIT 3) n WHERE a.id < 20 ORDER BY a.id";
    let p = plan(&mut s, sql);
    assert!(
        p.contains("Nested Loop (per row)") && p.contains("3 nearest v to"),
        "{p}"
    );
    let found = rows(&mut s, sql);
    assert_eq!(found.len(), 60);
    let exact = rows(
        &mut s,
        "SELECT a.id, n.id FROM items a, LATERAL (SELECT b.id FROM items b \
         WHERE b.id <> a.id ORDER BY (b.v <-> a.v) + 0 LIMIT 3) n WHERE a.id < 20 ORDER BY a.id",
    );
    let hits = found.iter().filter(|r| exact.contains(r)).count();
    assert!(hits >= 54, "{hits} of 60");
    // On worker threads, the same searches: the same rows.
    s.execute("SET parallel_workers = 4").unwrap();
    s.execute("SET batch_size = 64").unwrap();
    let before = nucleus_sql::vector::parallel_runs();
    assert_eq!(rows(&mut s, sql), found);
    assert!(nucleus_sql::vector::parallel_runs() > before);
    s.execute("RESET parallel_workers").unwrap();
    s.execute("RESET batch_size").unwrap();
    // LEFT JOIN LATERAL: a row without a vector is ranked by nothing (its
    // distances NULL), one with no other row keeps its row.
    s.execute("CREATE TABLE line (id INT PRIMARY KEY, v vector(1))")
        .unwrap();
    s.execute("CREATE INDEX line_v ON line USING hnsw (v)")
        .unwrap();
    s.execute("INSERT INTO line VALUES (1, '[0]'), (2, '[1]'), (3, NULL), (4, '[3]')")
        .unwrap();
    let sql = "SELECT a.id, n.id FROM line a LEFT JOIN LATERAL (SELECT b.id FROM line b \
               WHERE b.id <> a.id AND a.id < 4 ORDER BY b.v <-> a.v, b.id LIMIT 1) n ON true \
               ORDER BY a.id";
    assert_eq!(
        rows(&mut s, sql),
        [["1", "2"], ["2", "1"], ["3", "1"], ["4", "NULL"]]
    );

    // Writes: a deleted row is not found, a changed one where it is now.
    s.execute(&format!("UPDATE items SET v = '{q}' WHERE id = 77"))
        .unwrap();
    s.execute("DELETE FROM items WHERE id % 7 = 3").unwrap();
    let near = rows(
        &mut s,
        &format!("SELECT id, tag FROM items ORDER BY v <-> '{q}' LIMIT 10"),
    );
    assert_eq!(near[0][0], "77");
    assert!(near.iter().all(|r| r[1] != "t3"), "{near:?}");
    let ids = rows(
        &mut s,
        &format!("SELECT id FROM items {filter} ORDER BY v <-> '{q}' LIMIT 10"),
    );
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

/// A filter with a B-tree index (PLAN.md §M14.12 V5): few rows passing
/// are searched among exactly — the same rows as the exact search, through
/// writes not yet folded, changed vectors and deletes, before and after
/// the upkeep; many, through the graph, `ef_search` counting the rows that
/// pass; fewer than asked for, the range read and sorted.
#[test]
fn hnsw_searches_among_a_filters_rows() {
    let kv = db(17);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(5);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE items (id INT PRIMARY KEY, v vector({dims}), cat INT, tag TEXT)"
    ))
    .unwrap();
    let r: Vec<String> = (0..4000)
        .map(|i| format!("({i}, '{}', {}, 't{}')", g.vector(dims), i * 7 % 200, i % 3))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", r.join(", ")))
        .unwrap();
    s.execute("CREATE INDEX items_v ON items USING hnsw (v)")
        .unwrap();
    s.execute("CREATE INDEX items_cat ON items (cat)").unwrap();
    s.execute("ANALYZE").unwrap();
    let queries: Vec<String> = (0..10).map(|_| g.vector(dims)).collect();
    let q = &queries[0];
    let exact = |s: &mut Session, filter: &str, q: &str| -> Vec<Vec<String>> {
        rows(
            s,
            &format!(
                "SELECT id, (v <-> '{q}') + 0 AS d FROM items WHERE {filter} \
                 ORDER BY d, id LIMIT 10"
            ),
        )
    };
    let found = |s: &mut Session, filter: &str, q: &str| -> Vec<Vec<String>> {
        rows(
            s,
            &format!(
                "SELECT id, (v <-> '{q}') + 0 FROM items WHERE {filter} \
                 ORDER BY v <-> '{q}' LIMIT 10"
            ),
        )
    };
    // Searched among the range's rows: exactly the exact search's.
    let check = |s: &mut Session, ctx: &str| {
        for filter in [
            "cat = 7",
            "cat < 3",
            "cat = 7 AND tag = 't1'",
            "cat BETWEEN 4 AND 5",
        ] {
            let sql = format!("SELECT id FROM items WHERE {filter} ORDER BY v <-> '{q}' LIMIT 10");
            let p = plan(s, &sql);
            assert!(
                p.contains("nearest v to") && p.contains("among items_cat"),
                "{p}"
            );
            for q in &queries {
                let (f, e) = (found(s, filter, q), exact(s, filter, q));
                let d = |r: &Vec<Vec<String>>| r.iter().map(|x| x[1].clone()).collect::<Vec<_>>();
                assert_eq!(d(&f), d(&e), "{ctx}: {filter}");
            }
        }
    };
    check(&mut s, "built");
    // Rows in the delta, vectors changed (the old nodes stale), deletes.
    let r: Vec<String> = (4000..4100)
        .map(|i| format!("({i}, '{}', 7, 't1')", g.vector(dims)))
        .collect();
    s.execute(&format!("INSERT INTO items VALUES {}", r.join(", ")))
        .unwrap();
    s.execute(&format!(
        "UPDATE items SET v = '{q}' WHERE id IN (7 * 29, 2)"
    ))
    .unwrap();
    s.execute("DELETE FROM items WHERE id % 5 = 1 AND cat < 10")
        .unwrap();
    check(&mut s, "written");
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    check(&mut s, "folded");
    // Inside a transaction: its own writes.
    s.execute("BEGIN").unwrap();
    s.execute(&format!("INSERT INTO items VALUES (9000, '{q}', 7, 't1')"))
        .unwrap();
    assert_eq!(found(&mut s, "cat = 7", q)[0][0], "9000");
    s.execute("ROLLBACK").unwrap();
    assert_ne!(found(&mut s, "cat = 7", q)[0][0], "9000");

    // Fewer rows than asked for, one without a vector: the range read and
    // sorted, it last.
    s.execute(
        "INSERT INTO items VALUES (10001, '[0,0,0,0,0,0,0,1]', 500, 'x'), \
               (10002, NULL, 500, 'x'), (10003, '[0,0,0,0,0,0,0,2]', 500, 'x')",
    )
    .unwrap();
    let ids = rows(
        &mut s,
        "SELECT id FROM items WHERE cat = 500 ORDER BY v <-> '[0,0,0,0,0,0,0,0]' LIMIT 10",
    );
    assert_eq!(ids, [["10001"], ["10003"], ["10002"]]);
    let ids = rows(
        &mut s,
        "SELECT id FROM items WHERE cat = 501 ORDER BY v <-> '[0,0,0,0,0,0,0,0]' LIMIT 10",
    );
    assert!(ids.is_empty());

    // Many rows: through the graph, `ef_search` counted among the rows
    // that pass, so the recall holds whatever the filter's share — too
    // many expected (the range not read), or too many found (read, then
    // left for the graph).
    for filter in ["cat < 150", "cat < 60"] {
        let sql = format!("SELECT id FROM items WHERE {filter} ORDER BY v <-> '{q}' LIMIT 10");
        assert!(
            plan(&mut s, &sql).contains("nearest v to"),
            "{}",
            plan(&mut s, &sql)
        );
        let mut hit = 0.0;
        for q in &queries {
            let (f, e) = (found(&mut s, filter, q), exact(&mut s, filter, q));
            assert_eq!(f.len(), 10);
            let e: std::collections::HashSet<_> = e.iter().map(|x| x[0].clone()).collect();
            hit += f.iter().filter(|x| e.contains(&x[0])).count() as f64 / 10.0;
        }
        let recall = hit / queries.len() as f64;
        assert!(recall > 0.9, "{filter}: recall {recall}");
    }
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

/// Row-level security is a filter like any other (PLAN.md §M14.12 V5): a
/// search returns `k` rows the role may see, the nearest of those, and
/// evaluates no predicate of the statement on a row the policy hides.
#[test]
fn hnsw_searches_only_the_rows_a_policy_lets_through() {
    let kv = db(19);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(9);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, cat INT, v vector({dims})); \
         CREATE USER alice; CREATE USER bob; \
         GRANT SELECT ON docs TO alice; GRANT SELECT ON docs TO bob; \
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY own ON docs USING (owner = current_user)"
    ))
    .unwrap();
    // Alice has one row in four; bob's rows have a `cat` a statement's
    // predicate fails on.
    let r: Vec<String> = (0..20000)
        .map(|i| {
            let (owner, cat) = if i % 4 == 0 {
                ("alice", 1 + i % 7)
            } else {
                ("bob", 0)
            };
            format!("({i}, '{owner}', {cat}, '{}')", g.vector(dims))
        })
        .collect();
    s.execute(&format!("INSERT INTO docs VALUES {}", r.join(", ")))
        .unwrap();
    s.execute("CREATE INDEX docs_v ON docs USING hnsw (v)")
        .unwrap();
    let queries: Vec<String> = (0..10).map(|_| g.vector(dims)).collect();
    let exact = |s: &mut Session, filter: &str, q: &str| {
        rows(
            s,
            &format!("SELECT id FROM docs WHERE {filter} ORDER BY (v <-> '{q}') + 0, id LIMIT 10"),
        )
    };
    for owner_index in [false, true] {
        if owner_index {
            s.execute("CREATE INDEX docs_owner ON docs (owner)")
                .unwrap();
        }
        s.execute("ANALYZE").unwrap();
        for (filter, user) in [("true", "alice"), ("1 / cat > 0", "alice"), ("true", "bob")] {
            let truth: Vec<Vec<Vec<String>>> = queries
                .iter()
                .map(|q| exact(&mut s, &format!("owner = '{user}' AND {filter}"), q))
                .collect();
            s.execute(&format!("SET ROLE {user}")).unwrap();
            let sql = format!(
                "SELECT id FROM docs WHERE {filter} ORDER BY v <-> '{}' LIMIT 10",
                queries[0]
            );
            let p = plan(&mut s, &sql);
            assert!(p.contains("nearest v to"), "{p}");
            let mut hit = 0;
            for (q, t) in queries.iter().zip(&truth) {
                let found = rows(
                    &mut s,
                    &format!("SELECT id FROM docs WHERE {filter} ORDER BY v <-> '{q}' LIMIT 10"),
                );
                assert_eq!(found.len(), t.len(), "{user}, {filter}");
                hit += found.iter().filter(|f| t.contains(f)).count();
            }
            let recall = hit as f64 / (10 * queries.len()) as f64;
            assert!(recall > 0.9, "{user}, {filter}: recall {recall}");
            s.execute("RESET ROLE").unwrap();
        }
    }
}

/// `PARTITION BY` (PLAN.md §M14.12 V5): a graph per value of the columns,
/// searched where the conditions fix them (a policy's too) — the rows of
/// that value only, at the recall of a graph of its own — through
/// inserts, changed vectors, rows moved between values, deletes and the
/// upkeep; rows with a `NULL` value not indexed; the definition checked.
#[test]
fn hnsw_partitioned_by_a_column_searches_one_graph() {
    let kv = db(23);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(11);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE docs (id INT PRIMARY KEY, tenant INT, v vector({dims}))"
    ))
    .unwrap();
    let r: Vec<String> = (0..6000)
        .map(|i| {
            let tenant = if i % 100 == 99 {
                "NULL".to_string()
            } else {
                (i % 3).to_string()
            };
            format!("({i}, {tenant}, '{}')", g.vector(dims))
        })
        .collect();
    s.execute(&format!("INSERT INTO docs VALUES {}", r.join(", ")))
        .unwrap();
    // The definition: a method's index only; columns with keys.
    for (sql, code) in [
        ("CREATE INDEX x ON docs (v) PARTITION BY (tenant)", "0A000"),
        (
            "CREATE INDEX x ON docs USING hnsw (v) PARTITION BY (nope)",
            "42703",
        ),
        (
            "CREATE INDEX x ON docs USING hnsw (v) PARTITION BY (v)",
            "0A000",
        ),
        ("CREATE INDEX x ON docs USING hnsw (v, tenant)", "0A000"),
        (
            "CREATE INDEX x ON docs USING hnsw (v) PARTITION BY (tenant) WITH (partition_by = 1)",
            "22023",
        ),
    ] {
        let e = s.execute(sql).map(|_| sql).unwrap_err();
        assert_eq!(e.code, code, "{sql}: {}", e.message);
    }
    s.execute("CREATE INDEX docs_v ON docs USING hnsw (v) PARTITION BY (tenant)")
        .unwrap();
    let def = rows(
        &mut s,
        "SELECT options FROM nc_catalog.indexes WHERE name = 'docs_v'",
    );
    assert!(def[0][0].contains("partition_by=1"), "{def:?}");
    s.execute("ANALYZE").unwrap();
    let queries: Vec<String> = (0..10).map(|_| g.vector(dims)).collect();
    let exact = |s: &mut Session, filter: &str, q: &str| {
        rows(
            s,
            &format!("SELECT id FROM docs WHERE {filter} ORDER BY (v <-> '{q}') + 0, id LIMIT 10"),
        )
    };
    let check = |s: &mut Session, ctx: &str| {
        for tenant in 0..3 {
            let filter = format!("tenant = {tenant}");
            let sql = format!(
                "SELECT id FROM docs WHERE {filter} ORDER BY v <-> '{}' LIMIT 10",
                queries[0]
            );
            let p = plan(s, &sql);
            assert!(p.contains(&format!("in tenant = {tenant}")), "{p}");
            let mut hit = 0;
            for q in &queries {
                let found = rows(
                    s,
                    &format!("SELECT id FROM docs WHERE {filter} ORDER BY v <-> '{q}' LIMIT 10"),
                );
                let truth = exact(s, &filter, q);
                assert_eq!(found.len(), 10, "{ctx}");
                hit += found.iter().filter(|f| truth.contains(f)).count();
            }
            let recall = hit as f64 / 100.0;
            assert!(recall > 0.9, "{ctx}: tenant {tenant}: recall {recall}");
        }
        // No value fixed: not through the index.
        let sql = format!(
            "SELECT id FROM docs ORDER BY v <-> '{}' LIMIT 10",
            queries[0]
        );
        assert!(!plan(s, &sql).contains("nearest"), "{}", plan(s, &sql));
        assert_eq!(problems(&kv, s), Vec::<String>::new(), "{ctx}");
    };
    check(&mut s, "built");
    // A parameter fixes the value too.
    let q = &queries[0];
    let stmt = nucleus_sql::parser::parse(&format!(
        "SELECT id FROM docs WHERE tenant = $1 ORDER BY v <-> '{q}' LIMIT 10"
    ))
    .unwrap()
    .pop()
    .unwrap();
    match s.run(&stmt, &[nucleus_sql::Value::Int(1)]).unwrap() {
        Output::Rows { rows, .. } => assert_eq!(rows.len(), 10),
        other => panic!("{other:?}"),
    }
    // A tenant's new row nearest the query; rows moved to another
    // tenant, changed, deleted.
    s.execute(&format!("INSERT INTO docs VALUES (7000, 2, '{q}')"))
        .unwrap();
    s.execute("UPDATE docs SET tenant = 1 WHERE id % 3 = 0 AND id < 600")
        .unwrap();
    s.execute(&format!("UPDATE docs SET v = '{q}' WHERE id = 4"))
        .unwrap();
    s.execute("DELETE FROM docs WHERE id % 7 = 2").unwrap();
    s.execute("UPDATE docs SET tenant = NULL WHERE id = 8")
        .unwrap();
    let near = |s: &mut Session, tenant: i32| {
        rows(
            s,
            &format!("SELECT id FROM docs WHERE tenant = {tenant} ORDER BY v <-> '{q}' LIMIT 10"),
        )
    };
    assert_eq!(near(&mut s, 2)[0][0], "7000");
    assert_eq!(near(&mut s, 1)[0][0], "4");
    assert!(near(&mut s, 0).iter().all(|r| r[0] != "4"));
    check(&mut s, "written");
    nucleus_sql::hnsw::upkeep_now(&kv).unwrap();
    check(&mut s, "folded");
    // Row-level security fixes the value: a tenant's session.
    s.execute(
        "CREATE USER app; GRANT SELECT ON docs TO app; \
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY tenant ON docs USING (tenant = current_setting('app.tenant')::int)",
    )
    .unwrap();
    s.execute("SET ROLE app").unwrap();
    s.execute("SET app.tenant = '2'").unwrap();
    let sql = format!("SELECT id FROM docs ORDER BY v <-> '{q}' LIMIT 10");
    assert!(
        plan(&mut s, &sql).contains("in tenant = "),
        "{}",
        plan(&mut s, &sql)
    );
    assert_eq!(rows(&mut s, &sql)[0][0], "7000");
    s.execute("RESET ROLE").unwrap();
    // The partitioning column's type changed: the index defined again,
    // its graphs the same values'.
    s.execute("ALTER TABLE docs ALTER COLUMN tenant TYPE bigint")
        .unwrap();
    assert_eq!(near(&mut s, 2)[0][0], "7000");
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}

/// A partitioned table (§M14.20): an HNSW index on it is one on each
/// partition, a graph each; `ORDER BY … LIMIT k` searches each for `k`
/// rows and takes the nearest — the one partition its key fixes.
#[test]
fn hnsw_on_a_partitioned_table_searches_each_partition() {
    let kv = db(29);
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(13);
    let dims = 8;
    s.execute(&format!(
        "CREATE TABLE pts (id INT, region INT, v vector({dims}), PRIMARY KEY (region, id)) \
         PARTITION BY LIST (region); \
         CREATE TABLE pts_0 PARTITION OF pts FOR VALUES IN (0); \
         CREATE TABLE pts_1 PARTITION OF pts FOR VALUES IN (1); \
         CREATE TABLE pts_2 PARTITION OF pts FOR VALUES IN (2)"
    ))
    .unwrap();
    let r: Vec<String> = (0..9000)
        .map(|i| format!("({i}, {}, '{}')", i % 3, g.vector(dims)))
        .collect();
    s.execute(&format!("INSERT INTO pts VALUES {}", r.join(", ")))
        .unwrap();
    s.execute("CREATE INDEX pts_v ON pts USING hnsw (v)")
        .unwrap();
    s.execute("ANALYZE").unwrap();
    let queries: Vec<String> = (0..10).map(|_| g.vector(dims)).collect();
    for filter in ["true", "region = 1"] {
        let sql = format!(
            "SELECT id FROM pts WHERE {filter} ORDER BY v <-> '{}' LIMIT 10",
            queries[0]
        );
        let p = plan(&mut s, &sql);
        let searches = p.matches("nearest v to").count();
        assert_eq!(searches, if filter == "true" { 3 } else { 1 }, "{p}");
        let mut hit = 0;
        for q in &queries {
            let found = rows(
                &mut s,
                &format!("SELECT id FROM pts WHERE {filter} ORDER BY v <-> '{q}' LIMIT 10"),
            );
            let truth = rows(
                &mut s,
                &format!(
                    "SELECT id FROM pts WHERE {filter} ORDER BY (v <-> '{q}') + 0, id LIMIT 10"
                ),
            );
            assert_eq!(found.len(), 10);
            hit += found.iter().filter(|f| truth.contains(f)).count();
        }
        let recall = hit as f64 / 100.0;
        assert!(recall > 0.9, "{filter}: recall {recall}");
    }
    assert_eq!(problems(&kv, &s), Vec::<String>::new());
}
