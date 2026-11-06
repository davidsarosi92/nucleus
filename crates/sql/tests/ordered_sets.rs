//! Ordered-set aggregates (PLAN.md §M14.27 S5): `percentile_cont`,
//! `percentile_disc`, `mode` and `median` against their definitions —
//! the group's values sorted, the ranks read off — by groups and over a
//! whole table, in every executor: a row at a time, by batches, on
//! workers, and past `work_mem` (where numbers go to radix partitions,
//! text to a sort).

use std::cmp::Ordering;
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

const FRACTIONS: [f64; 7] = [0.0, 0.1, 0.37, 0.5, 0.75, 0.99, 1.0];

/// SQL's order of doubles: `NaN` the greatest.
fn cmp(a: &f64, b: &f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(b).unwrap(),
    }
}

/// `percentile_cont(f)` of values in the call's order.
fn cont(sorted: &[f64], f: f64) -> f64 {
    let m = (sorted.len() - 1) as f64;
    let pos = f * m;
    let (lo, hi) = (pos.floor(), pos.ceil());
    let (a, b) = (sorted[lo as usize], sorted[hi as usize]);
    if lo == hi {
        a
    } else {
        f.mul_add(m, -lo).mul_add(b - a, a)
    }
}

/// `percentile_disc(f)`'s rank in the call's order.
fn disc(n: usize, f: f64) -> usize {
    ((f * n as f64).ceil() as usize).max(1) - 1
}

fn same(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y))
        }
        (a, b) => a == b,
    }
}

#[test]
fn ordered_sets_are_their_definitions_in_every_setting() {
    let mut s = session(5);
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, g INT, x DOUBLE PRECISION, w TEXT);
         INSERT INTO t SELECT i, (i::bigint * 7919) % 23,
             CASE WHEN i % 13 = 0 THEN NULL
                  WHEN i % 997 = 0 THEN 'NaN'::float8
                  WHEN i % 1009 = 0 THEN '-Infinity'::float8
                  ELSE ((i::bigint * 104729) % 2000) / 16.0 - 40 END,
             'w' || ((i::bigint * 31) % 89 % (1 + i % 7))
           FROM generate_series(1, 30000) AS s(i)",
    )
    .unwrap();
    // The definitions, from the rows.
    let all = values(&mut s, "SELECT g, x, w FROM t");
    let mut groups: Vec<(i64, Vec<f64>, Vec<String>)> = Vec::new();
    for r in &all {
        let Value::Int(g) = r[0] else { panic!() };
        let i = match groups.iter().position(|e| e.0 == g) {
            Some(i) => i,
            None => {
                groups.push((g, Vec::new(), Vec::new()));
                groups.len() - 1
            }
        };
        if let Value::Float(x) = r[1] {
            groups[i].1.push(x);
        }
        if let Value::Text(w) = &r[2] {
            groups[i].2.push(w.clone());
        }
    }
    groups.sort_by_key(|e| e.0);
    let whole = (
        -1,
        groups.iter().flat_map(|e| e.1.clone()).collect::<Vec<_>>(),
        groups.iter().flat_map(|e| e.2.clone()).collect::<Vec<_>>(),
    );
    let want = |(_, xs, ws): &(i64, Vec<f64>, Vec<String>)| -> Vec<Value> {
        let mut asc = xs.clone();
        asc.sort_by(cmp);
        let desc: Vec<f64> = asc.iter().rev().copied().collect();
        let floats = |f: &dyn Fn(f64) -> f64| {
            Value::Array(FRACTIONS.iter().map(|&q| Value::Float(f(q))).collect())
        };
        let mut words = ws.clone();
        words.sort();
        // mode: the longest run, ties to the first (here: the least).
        let (mut best, mut best_n, mut i) = (String::new(), 0, 0);
        while i < words.len() {
            let j = (i..words.len())
                .find(|&j| words[j] != words[i])
                .unwrap_or(words.len());
            if j - i > best_n {
                (best, best_n) = (words[i].clone(), j - i);
            }
            i = j;
        }
        vec![
            floats(&|q| cont(&asc, q)),
            floats(&|q| cont(&desc, q)),
            floats(&|q| desc[disc(desc.len(), q)]),
            Value::Text(best),
            Value::Float(cont(&asc, 0.5)),
            Value::Text(words[disc(words.len(), 0.5)].clone()),
        ]
    };
    let calls = "percentile_cont(ARRAY[0, 0.1, 0.37, 0.5, 0.75, 0.99, 1]) WITHIN GROUP (ORDER BY x),
                 percentile_cont(ARRAY[0, 0.1, 0.37, 0.5, 0.75, 0.99, 1]) WITHIN GROUP (ORDER BY x DESC),
                 percentile_disc(ARRAY[0, 0.1, 0.37, 0.5, 0.75, 0.99, 1]) WITHIN GROUP (ORDER BY x DESC),
                 mode() WITHIN GROUP (ORDER BY w), median(x), median(w)";
    for setting in SETTINGS {
        s.execute(setting).unwrap();
        let got = values(
            &mut s,
            &format!("SELECT g, {calls} FROM t GROUP BY g ORDER BY g"),
        );
        assert_eq!(got.len(), groups.len());
        for (row, grp) in got.iter().zip(&groups) {
            let w = want(grp);
            for (k, (a, b)) in row[1..].iter().zip(&w).enumerate() {
                assert!(
                    same(a, b),
                    "{setting}: group {} call {k}: {a:?} != {b:?}",
                    grp.0
                );
            }
        }
        let got = values(&mut s, &format!("SELECT {calls} FROM t"));
        for (k, (a, b)) in got[0].iter().zip(&want(&whole)).enumerate() {
            assert!(
                same(a, b),
                "{setting}: whole table call {k}: {a:?} != {b:?}"
            );
        }
    }
}

#[test]
fn a_bad_fraction_is_an_error_where_it_is_known() {
    let mut s = session(6);
    s.execute("CREATE TABLE t (g INT, x DOUBLE PRECISION, f DOUBLE PRECISION)")
        .unwrap();
    // A constant: when the call is bound, rows or none.
    let e = s
        .execute("SELECT percentile_cont(2) WITHIN GROUP (ORDER BY x) FROM t")
        .unwrap_err();
    assert_eq!(e.code, "22003");
    // From the group's rows: when the group has some.
    s.execute("INSERT INTO t VALUES (1, 1, 0.5), (2, 2, 3)")
        .unwrap();
    let ok = values(
        &mut s,
        "SELECT g, percentile_cont(f) WITHIN GROUP (ORDER BY x) FROM t WHERE g = 1 GROUP BY g",
    );
    assert_eq!(ok, vec![vec![Value::Int(1), Value::Float(1.0)]]);
    let e = s
        .execute("SELECT g, percentile_cont(f) WITHIN GROUP (ORDER BY x) FROM t GROUP BY g")
        .unwrap_err();
    assert_eq!(e.code, "22003");
}
