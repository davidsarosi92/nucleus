//! Sketches (PLAN.md §M14.27 S6): `approx_count_distinct` / `hll`,
//! `approx_percentile` / `ddsketch`, `approx_top_k` — the same bytes in
//! every executor (a row at a time, by batches, on workers, past
//! `work_mem`: an aggregate over any split of its rows equals it over
//! one), and within their bounds of the exact answers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn values(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn session(seed: u64) -> Session {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(seed)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db);
    Session::new(kv)
}

const SETTINGS: [&str; 5] = [
    "SET batch_size = 0",
    "SET batch_size = 1024",
    "SET parallel_workers = 4",
    "SET work_mem = '64kB'",
    "SET parallel_workers = 1",
];

fn table(s: &mut Session) {
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, g INT, x DOUBLE PRECISION, w TEXT);
         INSERT INTO t SELECT i, i % 5,
             CASE WHEN i % 101 = 0 THEN NULL
                  WHEN i % 997 = 0 THEN 'NaN'::float8
                  WHEN i % 1009 = 0 THEN '-Infinity'::float8
                  ELSE (((i::bigint * 104729) % 20011) - 5000) * power(10::float8, (i % 9) - 4) END,
             'w' || ((i::bigint * 31) % 89 % (1 + i % 13))
           FROM generate_series(1, 40000) AS s(i)",
    )
    .unwrap();
}

#[test]
fn sketches_are_the_same_bytes_in_every_setting() {
    let mut s = session(7);
    table(&mut s);
    let calls = "hll_agg(id)::bytea, hll_agg(w)::bytea, ddsketch_agg(x)::bytea,
                 ddsketch_agg(x, 0.05)::bytea, approx_top_k(w, 4), approx_top_k_counts(w, 4),
                 approx_count_distinct(x), approx_percentile(x, ARRAY[0, 0.5, 0.99, 1])";
    let mut first: Option<(Vec<Vec<Value>>, Vec<Vec<Value>>)> = None;
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        let grouped = values(
            &mut s,
            &format!("SELECT g, {calls} FROM t GROUP BY g ORDER BY g"),
        );
        let whole = values(&mut s, &format!("SELECT {calls} FROM t"));
        // Merged from the groups' sketches: the whole table's.
        let merged = values(
            &mut s,
            "SELECT hll_merge(h)::bytea, ddsketch_merge(d)::bytea FROM
               (SELECT hll_agg(id) AS h, ddsketch_agg(x) AS d FROM t GROUP BY g) q",
        );
        assert_eq!(merged[0][0], whole[0][0], "{setting}: hll_merge");
        assert_eq!(merged[0][1], whole[0][2], "{setting}: ddsketch_merge");
        match &first {
            None => first = Some((grouped, whole)),
            Some((g, w)) => {
                assert_eq!(&grouped, g, "{setting}: by groups");
                assert_eq!(&whole, w, "{setting}: whole table");
            }
        }
    }
}

#[test]
fn sketches_are_within_their_bounds() {
    let mut s = session(8);
    table(&mut s);
    let rows = values(&mut s, "SELECT g, id, x, w FROM t");
    let mut xs: HashMap<i64, Vec<f64>> = HashMap::new();
    let mut ws: HashMap<i64, HashMap<String, u64>> = HashMap::new();
    for r in &rows {
        let Value::Int(g) = r[0] else { panic!() };
        if let Value::Float(x) = r[2] {
            xs.entry(g).or_default().push(x);
        }
        if let Value::Text(w) = &r[3] {
            *ws.entry(g).or_default().entry(w.clone()).or_default() += 1;
        }
    }
    let got = values(
        &mut s,
        "SELECT g, approx_count_distinct(id), approx_count_distinct(x), count(DISTINCT x),
                approx_percentile(x, ARRAY[0, 0.1, 0.5, 0.9, 0.99, 1]),
                approx_top_k(w, 3), approx_top_k_counts(w, 3)
           FROM t GROUP BY g ORDER BY g",
    );
    for r in &got {
        let Value::Int(g) = r[0] else { panic!() };
        // Distinct counts, both past the sparse form's 4096: within three
        // standard errors (0.81 % each).
        let (Value::Int(ids), Value::Int(dx), Value::Int(exact_dx)) = (&r[1], &r[2], &r[3]) else {
            panic!()
        };
        assert!(
            (*ids - 8000).abs() as f64 <= 3.0 * 0.0081 * 8000.0,
            "g {g}: {ids}"
        );
        assert!(
            (dx - exact_dx).abs() as f64 <= 3.0 * 0.0081 * *exact_dx as f64,
            "g {g}: {dx} of {exact_dx}"
        );
        // Quantiles: within 1 % of the value at rank ⌊f (n − 1)⌋.
        let mut v = xs[&g].clone();
        v.sort_by(f64::total_cmp);
        let Value::Array(qs) = &r[4] else { panic!() };
        for (q, f) in qs.iter().zip([0.0, 0.1, 0.5, 0.9, 0.99, 1.0]) {
            let Value::Float(q) = q else { panic!() };
            let want = v[(f * (v.len() - 1) as f64).floor() as usize];
            let close = q.to_bits() == want.to_bits()
                || (q - want).abs() <= 0.01 * want.abs() * (1.0 + 1e-9);
            assert!(close, "g {g} f {f}: {q} vs {want}");
        }
        // Few distinct words: exact, ties to the least.
        let mut counts: Vec<(&String, &u64)> = ws[&g].iter().collect();
        counts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let want_words: Vec<Value> = counts[..3]
            .iter()
            .map(|(w, _)| Value::Text((*w).clone()))
            .collect();
        let want_counts: Vec<Value> = counts[..3]
            .iter()
            .map(|(_, n)| Value::Int(**n as i64))
            .collect();
        assert_eq!(r[5], Value::Array(want_words), "g {g}");
        assert_eq!(r[6], Value::Array(want_counts), "g {g}");
    }
}
