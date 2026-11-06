//! Window functions (PLAN.md §M14.27 S4) against their definitions: each
//! frame's aggregate is what a brute-force pass over the rows the frame
//! names gives, whichever way it was evaluated — the running state, the
//! segment tree or a state per frame — and the same with every batch size,
//! worker count and `work_mem` (an external sort).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

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
        other => panic!("{sql}: {other:?}"),
    }
}

struct Row {
    id: i64,
    g: i64,
    /// The ordering value: unique within a partition (`u`) or with ties
    /// (`o`).
    u: i64,
    o: i64,
    v: Option<i64>,
    /// Its position by `u` and its peer group by `o`, in its partition.
    pos: i64,
    grp: i64,
}

#[derive(Clone, Copy)]
enum Units {
    Rows,
    Groups,
    Range,
}

/// Whether row `j` is in row `i`'s frame (`a` before to `b` after, as
/// signed offsets), by the definition.
fn in_frame(units: Units, part: &[&Row], i: usize, j: usize, a: i64, b: i64, excl: &str) -> bool {
    let key = |r: &Row| match units {
        Units::Rows => r.u,
        _ => r.o,
    };
    let (ki, kj) = (key(part[i]), key(part[j]));
    let inside = match units {
        Units::Rows => {
            let (pi, pj) = (part[i].pos, part[j].pos);
            pi - a <= pj && pj <= pi + b
        }
        Units::Groups => {
            let (gi, gj) = (part[i].grp, part[j].grp);
            gi - a <= gj && gj <= gi + b
        }
        Units::Range => ki - a <= kj && kj <= ki + b,
    };
    let peer = ki == kj;
    inside
        && match excl {
            "CURRENT ROW" => i != j,
            "GROUP" => !peer,
            "TIES" => !peer || i == j,
            _ => true,
        }
}

#[test]
fn frames_agree_with_their_definitions() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(11)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv.clone());
    let n = 700i64;
    let mut data: Vec<Row> = (1..=n)
        .map(|i| Row {
            id: i,
            g: i % 3,
            u: (i * 7919) % 1009,
            o: (i * 31) % 47,
            v: (i % 11 != 0).then_some((i * 104729) % 199 - 99),
            pos: 0,
            grp: 0,
        })
        .collect();
    for k in 0..data.len() {
        let (g, u, o) = (data[k].g, data[k].u, data[k].o);
        let part = || data.iter().filter(move |x| x.g == g);
        let mut below: Vec<i64> = part().map(|x| x.o).filter(|&x| x < o).collect();
        below.sort();
        below.dedup();
        data[k].pos = part().filter(|x| x.u < u).count() as i64;
        data[k].grp = below.len() as i64;
    }
    s.execute("CREATE TABLE w (id INT PRIMARY KEY, g INT, u INT, o INT, v INT)")
        .unwrap();
    let values: Vec<String> = data
        .iter()
        .map(|r| {
            let v = r.v.map_or("NULL".to_string(), |v| v.to_string());
            format!("({}, {}, {}, {}, {v})", r.id, r.g, r.u, r.o)
        })
        .collect();
    s.execute(&format!("INSERT INTO w VALUES {}", values.join(", ")))
        .unwrap();

    let bound = |x: i64, after: bool| match (x, after) {
        (i64::MAX, false) => "UNBOUNDED PRECEDING".to_string(),
        (i64::MAX, true) => "UNBOUNDED FOLLOWING".to_string(),
        (0, _) => "CURRENT ROW".to_string(),
        (x, false) if x > 0 => format!("{x} PRECEDING"),
        (x, false) => format!("{} FOLLOWING", -x),
        (x, true) if x > 0 => format!("{x} FOLLOWING"),
        (x, true) => format!("{} PRECEDING", -x),
    };
    let big = i64::MAX;
    // (units, a before, b after, exclusion)
    let mut cases = Vec::new();
    for units in [Units::Rows, Units::Groups, Units::Range] {
        for (a, b) in [
            (big, 0),
            (big, big),
            (3, 2),
            (0, 5),
            (7, -2),
            (-1, 4),
            (big, 6),
            (2, big),
        ] {
            for excl in ["", "CURRENT ROW", "GROUP", "TIES"] {
                cases.push((units, a, b, excl));
            }
        }
    }
    let mut want = Vec::new();
    let mut queries = Vec::new();
    for &(units, a, b, excl) in &cases {
        let (name, col) = match units {
            Units::Rows => ("ROWS", "u"),
            Units::Groups => ("GROUPS", "o"),
            Units::Range => ("RANGE", "o"),
        };
        let exclude = if excl.is_empty() {
            String::new()
        } else {
            format!(" EXCLUDE {excl}")
        };
        let w = format!(
            "(PARTITION BY g ORDER BY {col} {name} BETWEEN {} AND {}{exclude})",
            bound(a, false),
            bound(b, true)
        );
        // sum and count: a segment tree or a running state; string_agg: a
        // state per frame (its states do not combine).
        queries.push(format!(
            "SELECT id, sum(v) OVER {w}, count(v) OVER {w}, count(*) OVER {w}, \
             length(string_agg(v::text, ',') OVER {w}) FROM w ORDER BY id"
        ));
        let mut out = Vec::new();
        for r in &data {
            let part: Vec<&Row> = data.iter().filter(|x| x.g == r.g).collect();
            let i = part.iter().position(|x| x.id == r.id).unwrap();
            let (a, b) = (a.min(n), b.min(n));
            let frame: Vec<&Row> = (0..part.len())
                .filter(|&j| in_frame(units, &part, i, j, a, b, excl))
                .map(|j| part[j])
                .collect();
            let vs: Vec<i64> = frame.iter().filter_map(|x| x.v).collect();
            let sum = if vs.is_empty() {
                "NULL".to_string()
            } else {
                vs.iter().sum::<i64>().to_string()
            };
            let len = if vs.is_empty() {
                "NULL".to_string()
            } else {
                let text: usize = vs.iter().map(|v| v.to_string().len()).sum();
                (text + vs.len() - 1).to_string()
            };
            out.push(vec![
                r.id.to_string(),
                sum,
                vs.len().to_string(),
                frame.len().to_string(),
                len,
            ]);
        }
        want.push(out);
    }
    for (setup, label) in [
        ("SET batch_size = 0", "rows"),
        ("SET batch_size = 3", "batch 3"),
        ("SET parallel_workers = 8", "8 workers"),
        ("SET work_mem = '64kB'", "64kB work_mem"),
    ] {
        let mut s = Session::new(kv.clone());
        s.execute(setup).unwrap();
        for (q, w) in queries.iter().zip(&want) {
            assert_eq!(&rows(&mut s, q), w, "{label}: {q}");
        }
    }
}

#[test]
fn long_partitions_renew_their_frames_groups() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(12)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    // Past the 1024 frames an accumulator of scratch groups serves: the
    // running state, small frames and the segment tree each over 5000
    // rows.
    let got = rows(
        &mut s,
        "SELECT i, sum(i) OVER (ORDER BY i), \
         sum(i) OVER (ORDER BY i ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING), \
         avg(i) OVER (ORDER BY i ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING), \
         sum(i) OVER (ORDER BY i ROWS BETWEEN 100 PRECEDING AND 150 FOLLOWING) \
         FROM generate_series(1, 5000) i ORDER BY i",
    );
    assert_eq!(got.len(), 5000);
    for (k, r) in got.iter().enumerate() {
        let i = k as i64 + 1;
        let (lo, hi) = ((i - 2).max(1), (i + 1).min(5000));
        let window: i64 = (lo..=hi).sum();
        assert_eq!(r[1], (i * (i + 1) / 2).to_string(), "running, row {i}");
        assert_eq!(r[2], window.to_string(), "moving, row {i}");
        let avg: f64 = r[3].parse().unwrap();
        assert!(
            (avg - window as f64 / (hi - lo + 1) as f64).abs() < 1e-9,
            "avg, row {i}"
        );
        let wide: i64 = ((i - 100).max(1)..=(i + 150).min(5000)).sum();
        assert_eq!(r[4], wide.to_string(), "wide, row {i}");
    }
}

#[test]
fn running_frames_agree_with_the_other_ways() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(13)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE r (id INT PRIMARY KEY, g INT, i INT, f FLOAT, n NUMERIC(12, 3), t TEXT, b BOOL); \
         INSERT INTO r SELECT k, k % 4, (k * 7919) % 1009 - 500, \
           ((k * 104729) % 2003)::float8 / 7, ((k * 31) % 997)::numeric / 8, \
           'v' || ((k * 13) % 101), (k * 17) % 5 <> 0 FROM generate_series(1, 3000) k; \
         UPDATE r SET i = NULL, f = NULL, n = NULL, t = NULL, b = NULL WHERE id % 23 = 0",
    )
    .unwrap();
    // With a unique ordering, EXCLUDE TIES leaves the frame as it is but
    // cuts it in two, so it is not evaluated as a running frame: the
    // running state (finished in place, or a copy) against small frames
    // and the segment tree.
    for agg in [
        "count(*)",
        "count(i)",
        "sum(i)",
        "sum(f)",
        "sum(n)",
        "avg(i)",
        "avg(f)",
        "avg(n)",
        "min(i)",
        "max(t)",
        "min(n)",
        "bool_and(b)",
        "bool_or(b)",
        "variance(f)",
        "stddev(i)",
        "kurtosis(f)",
        "corr(f, i)",
        "regr_slope(i, f)",
        "array_agg(i)",
    ] {
        let q = |exclude: &str| {
            format!(
                "SELECT id, {agg} OVER (PARTITION BY g ORDER BY id ROWS UNBOUNDED PRECEDING{exclude}) \
                 FROM r ORDER BY id"
            )
        };
        let running = rows(&mut s, &q(""));
        let cut = rows(&mut s, &q(" EXCLUDE TIES"));
        assert_eq!(running, cut, "{agg}");
    }
}

#[test]
fn an_order_by_the_last_window_gives_is_not_sorted_again() {
    let db = Database::open_on(
        Arc::new(SimPlatform::new(14)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let mut s = Session::new(kv);
    s.execute("CREATE TABLE o (id INT PRIMARY KEY, g INT, x INT)")
        .unwrap();
    let sorts = |s: &mut Session, q: &str| {
        let plan = rows(s, &format!("EXPLAIN {q}"));
        plan.iter()
            .filter(|r| r[0].trim_start().starts_with("Sort:"))
            .count()
    };
    for (q, want) in [
        ("SELECT id, sum(x) OVER (ORDER BY x) FROM o ORDER BY x", 1),
        (
            "SELECT id, rank() OVER (PARTITION BY g ORDER BY x DESC) FROM o ORDER BY g, x DESC",
            1,
        ),
        (
            "SELECT id, rank() OVER (PARTITION BY g ORDER BY x DESC) FROM o ORDER BY g",
            1,
        ),
        (
            "SELECT id, rank() OVER (PARTITION BY g ORDER BY x) FROM o ORDER BY g, x DESC",
            2,
        ),
        (
            "SELECT id, rank() OVER (PARTITION BY g ORDER BY x) FROM o ORDER BY x",
            2,
        ),
        (
            "SELECT DISTINCT g, count(*) OVER (ORDER BY g) FROM o ORDER BY g",
            2,
        ),
        // The last window's order is the one left: lag's, not row_number's.
        (
            "SELECT id, row_number() OVER (ORDER BY x), lag(x) OVER (ORDER BY id) FROM o ORDER BY id",
            2,
        ),
        (
            "SELECT id, row_number() OVER (ORDER BY x), lag(x) OVER (ORDER BY id) FROM o ORDER BY x",
            3,
        ),
    ] {
        assert_eq!(sorts(&mut s, q), want, "{q}");
    }
}
