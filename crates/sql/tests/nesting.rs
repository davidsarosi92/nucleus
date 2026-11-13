//! Statements nested deep — parentheses, chained operators, subqueries,
//! set operations — run on a session's stack or fail with 54001; never
//! overflow it (which aborts the process: every session with it).  Found
//! by the sql_parse fuzz target (PLAN.md §M14.29 S2).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::parser::{MAX_NESTING, QUERY_WEIGHT};

fn shape(name: &str, n: usize) -> String {
    let rep = |s: &str, k: usize| s.repeat(k);
    match name {
        "parens" => format!("SELECT {}1{}", rep("(", n), rep(")", n)),
        "or" => format!("SELECT 1 WHERE {}", vec!["1=0"; n].join(" OR ")),
        "plus" => format!("SELECT {}", vec!["1"; n].join(" + ")),
        "not" => format!("SELECT {}true", rep("NOT ", n)),
        "neg" => format!("SELECT {}1", rep("- ", n)),
        "cast" => format!("SELECT 1{}", rep("::int", n)),
        "case" => format!(
            "SELECT {}1{}",
            rep("CASE WHEN true THEN ", n),
            rep(" END", n)
        ),
        "call" => format!("SELECT {}1{}", rep("abs(", n), rep(")", n)),
        "subquery" => format!("SELECT {}1{}", rep("(SELECT ", n), rep(")", n)),
        "from" => format!(
            "SELECT * FROM {}(SELECT 1 AS x) a{}",
            rep("(SELECT * FROM ", n),
            rep(") a", n)
        ),
        "union" => vec!["SELECT 1"; n].join(" UNION ALL "),
        "join" => format!(
            "SELECT 1 FROM (VALUES (1)) a0{}",
            (1..n)
                .map(|i| format!(" CROSS JOIN (VALUES (1)) a{i}"))
                .collect::<String>()
        ),
        _ => unreachable!(),
    }
}

/// Runs each statement on a session's thread (its stack as the server's).
fn outcomes(statements: Vec<(String, String)>) -> Vec<(String, Result<(), String>)> {
    nucleus_platform::session_thread("nesting")
        .spawn(move || {
            let db = Database::open_on(
                Arc::new(SimPlatform::new(1)),
                Path::new("/db"),
                Options::default(),
            )
            .unwrap();
            let kv: Arc<dyn Kv> = Arc::new(db);
            let mut s = Session::new(kv);
            statements
                .into_iter()
                .map(|(name, sql)| {
                    (
                        name,
                        s.execute(&sql).map(|_| ()).map_err(|e| e.code.to_string()),
                    )
                })
                .collect()
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn too_deep_fails_with_54001() {
    let names = [
        "parens", "or", "plus", "not", "neg", "cast", "case", "call", "subquery", "from", "union",
    ];
    let deep = |name: &str| shape(name, if name == "union" { 20_000 } else { 50_000 });
    let got = outcomes(names.iter().map(|n| (n.to_string(), deep(n))).collect());
    for (name, r) in got {
        assert_eq!(r, Err("54001".to_string()), "{name}");
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
// A 32-bit session's stack is half a 64-bit one's: only the 64-bit one is
// measured up to the limit.
#[cfg_attr(target_pointer_width = "32", ignore = "measured on 64-bit")]
fn up_to_the_limit_runs() {
    // Just under the limit, each shape (the query around it counts too).
    let under = |name: &str| match name {
        "subquery" | "from" => (MAX_NESTING - 2 * QUERY_WEIGHT) / (QUERY_WEIGHT + 3),
        "union" => 300,
        "join" => 500,
        _ => MAX_NESTING - QUERY_WEIGHT - 4,
    };
    let names = [
        "parens", "or", "plus", "not", "neg", "cast", "case", "call", "subquery", "from", "union",
        "join",
    ];
    let got = outcomes(
        names
            .iter()
            .map(|n| (n.to_string(), shape(n, under(n))))
            .collect(),
    );
    for (name, r) in got {
        assert_eq!(r, Ok(()), "{name} at {}", under(&name));
    }
}
